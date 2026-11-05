//! The radio half of wartui's P4 split bridge, on the ESP32-C6 of a Waveshare ESP32-P4-WIFI6.
//!
//! The P4 owns the USB link to the host. This chip owns the radio: it parks on the fleet's
//! control channel, queues every wartui frame it hears for the P4, and carries out the
//! radio commands the P4 relays. It never talks USB. The two talk over the board's SDIO
//! traces, on UART1 with hardware flow control, through UHCI's DMA (`src/link.rs`).
//!
//! The ESP-NOW half is [`wartui_firmware::radio`], kept apart from this chip's link. Updates
//! arrive from the P4 over the link and boot on trial, through
//! [`wartui_firmware::ota`], the flash code a node uses. Three futures share one task: the
//! radio's jobs, `hear_loop` draining the receive queue, and the link. Each waits on an
//! interrupt, a signal or a timer rather than polling.
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

use core::cell::RefCell;

use embassy_futures::join::join3;
use esp_hal::clock::CpuClock;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuf, DmaTxBuf, descriptor_count};
use esp_hal::gpio::{Input, InputConfig};
use esp_hal::time::Instant;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::uhci::{self, Uhci};
use esp_hal::uart::{
    BaudrateTolerance, Config as UartConfig, CtsConfig, HwFlowControl, RtsConfig,
    RxConfig as UartRxConfig, Uart,
};
use esp_radio::esp_now::{EspNowReceiver, ReceivedData};
use esp_radio::wifi::{ControllerConfig, Interface, WifiController};
use static_cell::ConstStaticCell;
use wartui_firmware::ota::Ota;
use wartui_firmware::phase::{boot_phase, mark};
use wartui_firmware::radio::{self, Dones, Heard, Jobs, Payloads};
use wartui_firmware::{phase, reboot, reset_cause};
use wartui_proto::air::Board;
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::link::LoopPhase;
use wartui_proto::ota::Receiver;
use wartui_proto::radio_link::{
    BAUD_RATE, BODY_MAX, FIFO_FULL_THRESHOLD, Held, PacketReader, RTS_THRESHOLD, RX_BUF_LEN, Up,
    WIRE_MAX,
};

use crate::config::{RADIO_JOBS, SEND_JOBS};
use crate::link::{Bus, Link, Radio, Shared, UpPriority, UpQueue, Updates, link_loop};

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(not(feature = "esp32c6"))]
compile_error!("build with --features esp32c6: the P4 boards' radio is an ESP32-C6");

/// A DMA buffer of `$len` bytes, with its descriptors, in static internal RAM. Each use has
/// its own statics.
macro_rules! dma_buf {
    ($ty:ident, $len:expr) => {{
        const LEN: usize = $len;
        const DESCRIPTORS: usize =
            descriptor_count(LEN, BurstConfig::DEFAULT.max_compatible_chunk_size());
        static BUFFER: ConstStaticCell<InternalMemory<[u8; LEN]>> =
            ConstStaticCell::new(InternalMemory::new([0; LEN]));
        static DESCRIPTOR_LIST: ConstStaticCell<InternalMemory<[DmaDescriptor; DESCRIPTORS]>> =
            ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; DESCRIPTORS]));
        let mut buf =
            $ty::new(DESCRIPTOR_LIST.take().get_mut().unsize(), BUFFER.take().get_mut().unsize())
                .expect("DMA buffer");
        buf.set_length(LEN);
        buf
    }};
}

/// Resets rather than hanging. The P4 sees the new life in the next hello and announces a
/// fresh `Ready`, so the host re-adds peers and transmit power.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    reboot()
}

/// Received frames waiting for the P4. Built at compile time, so the ring is never on the
/// stack.
static UP_QUEUE: ConstStaticCell<UpQueue> = ConstStaticCell::new(UpQueue::new(FrameRing::new()));

/// Every other record waiting for the P4, sent ahead of the received frames.
static UP_PRIORITY: ConstStaticCell<UpPriority> =
    ConstStaticCell::new(UpPriority::new(FrameRing::new()));

/// The record that did not fit the last packet.
static HELD: ConstStaticCell<Held> = ConstStaticCell::new(Held::new());

/// The packet the reader is part-way through.
static READER: ConstStaticCell<PacketReader> = ConstStaticCell::new(PacketReader::new());

/// Where the writer builds a packet's body.
static BODY: ConstStaticCell<[u8; BODY_MAX]> = ConstStaticCell::new([0; BODY_MAX]);

/// The radio jobs [`radio::run_jobs`] works through, in arrival order.
static JOBS: ConstStaticCell<Jobs<RADIO_JOBS>> = ConstStaticCell::new(Jobs::new());

/// The payloads of the sends among the jobs; each is a full ESP-NOW v2 payload.
static PAYLOADS: ConstStaticCell<Payloads<SEND_JOBS>> = ConstStaticCell::new(Payloads::new());

/// What became of each job, waiting for the link.
static DONES: ConstStaticCell<Dones<RADIO_JOBS>> = ConstStaticCell::new(Dones::new());

/// The update receiver, with its sector of buffer.
static RECEIVER: ConstStaticCell<Receiver> = ConstStaticCell::new(Receiver::new(Board::Esp32c6));

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let boot = Instant::now();

    // Read before anything else writes it: this is the previous life's marker.
    let cause = reset_cause();
    let last_phase = boot_phase(cause);
    mark(LoopPhase::Boot);
    phase::claim();

    // Before the radio, so an image that has run out of trial boots is rolled back before
    // it does anything else. What it says waits for the link.
    let (mut ota, table_fault) = Ota::new(peripherals.FLASH);
    let booted = ota.boot();
    if booted.reboot {
        reboot()
    }
    // Zeros name no image, so the P4 still sends its own.
    let image = ota.booted_digest().unwrap_or([0; 32]);

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // `US` rather than `esp-radio`'s default, as every wartui firmware passes;
    // `firmware/node/src/main.rs` has why.
    let mut controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");
    let (manager, mut sender, mut receiver) = controller.esp_now().split();
    let setup = radio::setup(&mut controller, &manager);
    let mac = Interface::station().mac_address();

    // SDIO D2 and D3 are unused; both ends leave them as inputs.
    let _d2 = Input::new(peripherals.GPIO22, InputConfig::default());
    let _d3 = Input::new(peripherals.GPIO23, InputConfig::default());
    // UART0 is the flash port. Refused rather than run slightly off, since the P4 is not.
    let uart = Uart::new(
        peripherals.UART1,
        UartConfig::default()
            .with_baudrate(BAUD_RATE)
            .with_baudrate_tolerance(BaudrateTolerance::Exact)
            .with_hw_flow_ctrl(HwFlowControl {
                cts: CtsConfig::Enabled,
                rts: RtsConfig::Enabled(RTS_THRESHOLD),
            })
            .with_rx(UartRxConfig::default().with_fifo_full_threshold(FIFO_FULL_THRESHOLD)),
    )
    .expect("UART1")
    .with_tx(peripherals.GPIO20)
    .with_rx(peripherals.GPIO18)
    .with_rts(peripherals.GPIO21)
    .with_cts(peripherals.GPIO19);
    let mut uhci = Uhci::new(uart, peripherals.UHCI0, peripherals.DMA_CH0);
    uhci.apply_rx_config(&uhci::RxConfig::default().with_chunk_limit(RX_BUF_LEN as u16))
        .expect("UHCI read limit");
    // Despite its name, this config sets when a read ends: at the limit, or on an idle line.
    // `Uhci::new` clears both.
    uhci.apply_tx_config(&uhci::TxConfig::default()).expect("UHCI end of read");
    let (rx, tx) = uhci.into_async().split();
    let bus = Bus {
        rx,
        tx,
        rx_bufs: [dma_buf!(DmaRxBuf, RX_BUF_LEN), dma_buf!(DmaRxBuf, RX_BUF_LEN)],
        tx_buf: dma_buf!(DmaTxBuf, WIRE_MAX),
        reader: READER.take(),
        body: BODY.take(),
    };

    let shared = Shared::new(UP_QUEUE.take(), UP_PRIORITY.take());
    // Queued behind the hello exchange, for the P4 to relay once the link is up.
    for said in setup.said {
        shared.say(said);
    }
    if let Some(err) = table_fault {
        shared
            .line(None, format_args!("could not read the partition table: {err}; updates refused"));
    } else if let Some(lacks) = ota.lacks() {
        shared.line(None, format_args!("{lacks}; updates refused"));
    }
    for note in &booted.notes {
        shared.note(note);
    }
    let updates =
        Updates { ota, receiver: RECEIVER.take(), trial: booted.trial, image, status: None };
    let link = RefCell::new(Link::new(boot, mac, cause, last_phase, HELD.take(), updates));
    let jobs = JOBS.take();
    let payloads = PAYLOADS.take();
    let dones = DONES.take();
    let radio = Radio { manager: &manager, jobs, payloads };

    // Joined rather than spawned: all three borrow the ESP-NOW handles `main` owns, and
    // none ends.
    match join3(
        radio::run_jobs(
            &mut controller,
            &manager,
            &mut sender,
            setup.tx_power,
            jobs,
            payloads,
            dones,
        ),
        hear_loop(&mut receiver, &shared, boot),
        link_loop(bus, &link, &shared, radio, dones),
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
    match radio::hear(received, rx_us, |rx| shared.push(&Up::Message(rx.clone()))) {
        Heard::Forwarded => shared.note_rx(),
        Heard::Oversize => shared.note_oversize(),
        Heard::Foreign => {}
    }
}
