//! The radio half of wartui's P4 split bridge, on the board's ESP32-C6.
//!
//! The P4 owns the USB link to the host. This chip owns the radio: it parks on the fleet's
//! control channel, queues every wartui frame it hears for the P4, and carries out the
//! radio commands the P4 relays. It never talks USB. The two talk over the board's SDIO
//! traces, with this chip as a 40 MHz SPI slave (`src/link.rs`).
//!
//! The ESP-NOW half is [`wartui_firmware::radio`], the single-chip bridge's own, so the two
//! bridges cannot drift. Three futures share one task: the radio's sends, `hear_loop`
//! draining the receive queue, and the link. Each waits on an interrupt rather than
//! polling, but for the one call `src/link.rs` names.
//!
//! `README.md` has the build command.
#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe with esp-hal types, especially those holding \
    buffers for the duration of a transfer"
)]
#![deny(clippy::large_stack_frames)]

mod config;
mod link;

use embassy_futures::join::join3;
use esp_hal::clock::CpuClock;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuf, DmaTxBuf, descriptor_count};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::spi::Mode;
use esp_hal::spi::slave::Spi;
use esp_hal::time::Instant;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::esp_now::{EspNowReceiver, ReceivedData};
use esp_radio::wifi::{ControllerConfig, Interface, WifiController};
use static_cell::ConstStaticCell;
use wartui_firmware::phase::{boot_phase, mark};
use wartui_firmware::radio::{self, Heard, SendDones, SendJobs};
use wartui_firmware::{phase, reboot, reset_cause};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::link::LoopPhase;
use wartui_proto::spi::{TRANSFER_LEN, Up};

use crate::config::SEND_JOBS;
use crate::link::{Bus, Link, RECORD_MAX, Radio, Shared, UpQueue, link_loop};

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(not(feature = "esp32c6"))]
compile_error!("build with --features esp32c6: the P4 boards' radio is an ESP32-C6");

/// A DMA buffer of one transfer, with its descriptors, in static internal RAM. Each use
/// has its own statics.
macro_rules! dma_buf {
    ($ty:ident) => {{
        const DESCRIPTORS: usize =
            descriptor_count(TRANSFER_LEN, BurstConfig::DEFAULT.max_compatible_chunk_size());
        static BUFFER: ConstStaticCell<InternalMemory<[u8; TRANSFER_LEN]>> =
            ConstStaticCell::new(InternalMemory::new([0; TRANSFER_LEN]));
        static DESCRIPTOR_LIST: ConstStaticCell<InternalMemory<[DmaDescriptor; DESCRIPTORS]>> =
            ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; DESCRIPTORS]));
        let mut buf =
            $ty::new(DESCRIPTOR_LIST.take().get_mut().unsize(), BUFFER.take().get_mut().unsize())
                .expect("DMA buffer");
        buf.set_length(TRANSFER_LEN);
        buf
    }};
}

/// Resets rather than hanging. The P4 sees the new life in the next hello and announces a
/// fresh `Ready`, so the host re-adds peers and transmit power.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    reboot()
}

/// Records waiting for the P4. Built at compile time, so the ring is never on the stack.
static UP_QUEUE: ConstStaticCell<UpQueue> = ConstStaticCell::new(UpQueue::new(FrameRing::new()));

/// The record that did not fit the last transfer.
static HELD: ConstStaticCell<[u8; RECORD_MAX]> = ConstStaticCell::new([0; RECORD_MAX]);

/// The send jobs [`radio::run_sends`] works through; each holds a full ESP-NOW v2 payload.
static SEND_JOBS_QUEUE: ConstStaticCell<SendJobs<SEND_JOBS>> =
    ConstStaticCell::new(SendJobs::new());

/// What became of each send, waiting for the link.
static SEND_DONES: ConstStaticCell<SendDones<SEND_JOBS>> = ConstStaticCell::new(SendDones::new());

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let boot = Instant::now();

    // Read before anything else writes it: this is the previous life's marker.
    let cause = reset_cause();
    let last_phase = boot_phase(cause);
    mark(LoopPhase::Boot);
    phase::claim();

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // `US` rather than `esp-radio`'s default, as every wartui firmware passes;
    // `firmware/bridge/src/main.rs` has why.
    let mut controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");
    let (manager, mut sender, mut receiver) = controller.esp_now().split();
    let setup = radio::setup(&mut controller, &manager);
    let mac = Interface::station().mac_address();

    // Low until the first transfer is armed.
    let ready = Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default());
    // One pin, two readers: the slave takes it as CS, and the link waits on its rising
    // edge for the end of each transfer.
    let cs = Input::new(peripherals.GPIO21, InputConfig::default());
    let spi = Spi::new(peripherals.SPI2, Mode::_0)
        .with_sck(peripherals.GPIO19)
        .with_mosi(peripherals.GPIO18)
        .with_miso(peripherals.GPIO20)
        .with_cs(cs.peripheral_input())
        .with_dma(peripherals.DMA_CH0);
    let bus = Bus {
        spi,
        cs,
        ready,
        rx: [dma_buf!(DmaRxBuf), dma_buf!(DmaRxBuf)],
        tx: [dma_buf!(DmaTxBuf), dma_buf!(DmaTxBuf)],
    };

    let shared = Shared::new(UP_QUEUE.take());
    // Queued behind the hello exchange, for the P4 to relay once the link is up.
    for said in setup.said {
        shared.say(said);
    }
    let mut link = Link::new(boot, mac, cause, last_phase, setup.tx_power, HELD.take());
    let jobs = SEND_JOBS_QUEUE.take();
    let dones = SEND_DONES.take();
    let radio = Radio { controller: &mut controller, manager: &manager, jobs };

    // Joined rather than spawned: all three borrow the ESP-NOW handles `main` owns, and
    // none ends.
    match join3(
        radio::run_sends(&manager, &mut sender, jobs, dones),
        hear_loop(&mut receiver, &shared, boot),
        link_loop(bus, &mut link, &shared, radio, dones),
    )
    .await {}
}

/// Queue every wartui frame the radio hears for the P4.
async fn hear_loop(receiver: &mut EspNowReceiver, shared: &Shared, boot: Instant) -> ! {
    loop {
        let received = receiver.receive_async().await;
        hear(&received, shared, boot);
    }
}

/// Queue one received frame, if it is ours.
///
/// Out of line because it builds a 1048-byte message. `radio::hear` has what is ours;
/// somebody else's frame is neither queued nor counted.
#[inline(never)]
fn hear(received: &ReceivedData, shared: &Shared, boot: Instant) {
    // On this chip's clock, wrapping after roughly 71 minutes, as `tx_us` is.
    let rx_us = boot.elapsed().as_micros() as u32;
    match radio::hear(received, rx_us) {
        Heard::Forward(rx) => {
            shared.note_rx();
            shared.push(&Up::Message(rx));
        }
        Heard::Oversize => shared.note_oversize(),
        Heard::Foreign => {}
    }
}
