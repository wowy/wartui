//! The host half of wartui's P4 split bridge, on a Waveshare ESP32-P4 board.
//!
//! The P4 talks to the host over USB 2.0 high speed with wartui's link protocol, and to
//! the board's ESP32-C6 as a 40 MHz SPI master. The C6 runs the radio
//! (`firmware/p4-radio`). The host cannot tell the pair from a single-chip bridge except
//! by `Chip::Esp32P4` and the extra diagnostics; [`wartui_proto::split::Pair`] makes the
//! decisions that keep it so.
//!
//! The futures share one task through `join`, so they share [`Shared`] through `Cell` and
//! `RefCell` with no locking and nothing borrowed across an `.await`:
//!
//! - **the SPI link** (`src/link.rs`) runs from boot, so the C6's hello arrives, and sends
//!   the C6 the image this P4 carries (`src/radio.rs`) when it runs another;
//! - **the radio clock** gives [`Pair`] the time every [`RADIO_TICK`], which is how it gives
//!   up on a silent C6, and resets one silent since an update's install through its enable
//!   pin;
//! - **the USB device** (`src/usb.rs`) starts once the pair knows its serial number, and
//!   runs the device, a reader, a writer and the `GetStatus` answer;
//! - **the GPS reader** (`src/gps.rs`, with the default `gps` feature) polls a u-blox
//!   receiver on I2C0 and forwards its bytes. GPIO7 and GPIO8 are shared with the board's
//!   ES8311 codec, which this firmware never addresses.
//!
//! The P4 resets the C6 through its enable pin at boot, so the pair always starts
//! together. Every wait is a GPIO interrupt, a DMA completion, a USB event, a signal
//! between the futures or a timer.
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
#[cfg(feature = "gps")]
mod gps;
mod link;
mod radio;
mod usb;

use core::cell::{Cell, RefCell};

#[cfg(feature = "gps")]
use embassy_futures::join::join;
use embassy_futures::join::join3;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
#[cfg(feature = "gps")]
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuf, DmaTxBuf, descriptor_count};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use static_cell::ConstStaticCell;
use wartui_firmware::phase::{boot_phase, mark};
use wartui_firmware::{phase, reboot, reset_cause};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::host_queue::{HostQueue, is_priority};
use wartui_proto::link::{
    BridgeToHost, LoopPhase, MAX_FRAME, QueueId, QueueStat, ShortStr, SpiStats, encode_frame,
};
#[cfg(feature = "gps")]
use wartui_proto::link::{GpsBytes, GpsWriteBytes, LogLevel, LogStr};
use wartui_proto::spi::{Down, Held, RECORD_MAX, TRANSFER_LEN, encode_down};
use wartui_proto::split::{Pair, PairEvent};

use crate::config::{DOWN_QUEUE_BYTES, HOST_BULK_BYTES, HOST_PRIORITY_BYTES, SPI_KHZ};
use crate::link::{Bus, Link, run_link};
use crate::radio::RADIO_DIGEST;

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

/// How long the C6 is held in reset, at boot and when the pair asks.
const C6_RESET_HOLD: Duration = Duration::from_millis(50);

/// How often [`Pair`] is given the time.
const RADIO_TICK: Duration = Duration::from_millis(100);

type HostFrames = HostQueue<HOST_PRIORITY_BYTES, HOST_BULK_BYTES>;
type DownQueue = FrameRing<DOWN_QUEUE_BYTES>;

/// Frames waiting for the host. Built at compile time, so the rings are never on the stack.
static HOST_QUEUE: ConstStaticCell<HostFrames> = ConstStaticCell::new(HostQueue::new());
/// `Down` records waiting for the C6.
static DOWN_QUEUE: ConstStaticCell<DownQueue> = ConstStaticCell::new(FrameRing::new());
/// The record that did not fit the last transfer.
static HELD: ConstStaticCell<Held> = ConstStaticCell::new(Held::new());

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

/// Resets rather than hanging. The reset takes the C6 with it, and the host sees a new
/// life in the next `Ready`.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    reboot()
}

/// Milliseconds on the P4's clock, the time [`Pair`] runs on.
fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// What the futures share. They are joined in one task, so nothing here is borrowed
/// across an `.await`.
pub struct Shared {
    pair: RefCell<Pair>,
    host: RefCell<&'static mut HostFrames>,
    /// Raised by every frame queued for the host, so an idle writer wakes.
    pushed: Signal<NoopRawMutex, ()>,
    down: RefCell<&'static mut DownQueue>,
    /// Records refused because the down queue was full. The ring counts only its own
    /// drops, and it never evicts here.
    down_refused: Cell<u32>,
    spi: Cell<SpiStats>,
    /// A host holds DTR, so it is reading.
    dtr: Cell<bool>,
    /// Raised on every new pair life, so the USB side learns its serial number.
    paired: Signal<NoopRawMutex, ()>,
    /// The host asked for a `Status`.
    status_asked: Signal<NoopRawMutex, ()>,
    /// The C6's counters arrived.
    radio_status: Signal<NoopRawMutex, ()>,
    /// Why no GPS receiver is being read. Sent with every announcement, because a host
    /// usually attaches after boot, and cleared when the receiver answers again.
    #[cfg(feature = "gps")]
    gps_fault: Cell<Option<&'static str>>,
    /// The GPS reader's write channel, once its bus is up.
    #[cfg(feature = "gps")]
    gps_writes: RefCell<Option<Sender<'static, NoopRawMutex, GpsWriteBytes>>>,
}

impl Shared {
    fn new(pair: Pair, host: &'static mut HostFrames, down: &'static mut DownQueue) -> Self {
        Self {
            pair: RefCell::new(pair),
            host: RefCell::new(host),
            pushed: Signal::new(),
            down: RefCell::new(down),
            down_refused: Cell::new(0),
            spi: Cell::new(SpiStats {
                transfers: 0,
                crc_fail: 0,
                bad_magic: 0,
                seq_gap: 0,
                ready_timeouts: 0,
                ready_recovered: 0,
                radio_crc_fail: 0,
                radio_bad_magic: 0,
                radio_seq_gap: 0,
            }),
            dtr: Cell::new(false),
            paired: Signal::new(),
            status_asked: Signal::new(),
            radio_status: Signal::new(),
            #[cfg(feature = "gps")]
            gps_fault: Cell::new(None),
            #[cfg(feature = "gps")]
            gps_writes: RefCell::new(None),
        }
    }

    /// Add one to the SPI counter `field` picks.
    fn count_spi(&self, field: impl FnOnce(&mut SpiStats) -> &mut u32) {
        let mut spi = self.spi.get();
        let counter = field(&mut spi);
        *counter = counter.wrapping_add(1);
        self.spi.set(spi);
    }

    /// Queue `msg` for the host. A full ring evicts its oldest frame and counts the drop.
    ///
    /// Dropped, and not counted, while no host holds DTR. A backlog held for an absent host
    /// arrives late when one attaches, carries its current position onto old sightings and
    /// reads as missed admin windows. Faults worth keeping are repeated by
    /// [`Shared::announce`], and the writer clears the queue when DTR rises (`src/usb.rs`).
    ///
    /// Out of line, with the encode buffer, to keep both out of the callers' frames.
    #[inline(never)]
    fn push_host(&self, msg: &BridgeToHost) {
        if !self.dtr.get() {
            return;
        }
        let mut frame = [0; MAX_FRAME];
        // Every `BridgeToHost` fits `MAX_FRAME`.
        let Ok(len) = encode_frame(msg, &mut frame) else { return };
        self.host.borrow_mut().push(&frame[..len], is_priority(msg));
        self.pushed.signal(());
    }

    /// Queue `down` for the C6, or refuse it and return `false` when the queue is full.
    ///
    /// Refused rather than evicting the oldest: an evicted command is lost with nothing to
    /// say so, and an evicted send would never be answered.
    #[inline(never)]
    fn push_down(&self, down: &Down) -> bool {
        // The largest `Down` is a relayed 1470-byte ESP-NOW send.
        let mut record = [0; RECORD_MAX];
        let mut ring = self.down.borrow_mut();
        // Each record sits behind the ring's two-byte length.
        let fits = |len: usize| DownQueue::CAPACITY - ring.len_bytes() >= 2 + len;
        let queued = match encode_down(down, &mut record) {
            Ok(len) if fits(len) => ring.push(&record[..len]),
            _ => false,
        };
        if !queued {
            self.down_refused.set(self.down_refused.get().wrapping_add(1));
        }
        queued
    }

    /// Announce this bridge life: `Ready`, then the fault if the radio is not usable, and
    /// the GPS fault if there is one.
    fn announce(&self) {
        self.push_ready();
        self.push_fault();
        #[cfg(feature = "gps")]
        if let Some(why) = self.gps_fault.get() {
            self.push_error(why);
        }
    }

    /// Out of line, as each message is, so the two never share a frame.
    #[inline(never)]
    fn push_ready(&self) {
        let ready = self.pair.borrow().ready(now_ms());
        if let Some(ready) = ready {
            self.push_host(&ready);
        }
    }

    #[inline(never)]
    fn push_fault(&self) {
        let fault = self.pair.borrow().fault();
        if let Some(message) = fault {
            self.push_host(&BridgeToHost::Error { message });
        }
    }

    #[cfg(feature = "gps")]
    #[inline(never)]
    fn push_error(&self, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.push_host(&BridgeToHost::Error { message });
    }

    #[cfg(feature = "gps")]
    #[inline(never)]
    fn push_log(&self, level: LogLevel, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.push_host(&BridgeToHost::Log { level, message });
    }

    /// Queue a batch from the GPS receiver, copied out of its slot here rather than in the
    /// reader's future.
    #[cfg(feature = "gps")]
    #[inline(never)]
    fn push_gps(&self, bytes: &GpsBytes) {
        self.push_host(&BridgeToHost::Gps { bytes: bytes.clone() });
    }

    /// A new pair life began: tell the USB side, and a host that is reading.
    fn new_life(&self) {
        self.paired.signal(());
        self.announce();
    }

    /// Queue the `Status` a poll asked for.
    #[inline(never)]
    fn push_status(&self) {
        let host = self.host.borrow().stats();
        let down = {
            let ring = self.down.borrow();
            QueueStat {
                queue: QueueId::RadioDown,
                capacity: DOWN_QUEUE_BYTES as u32,
                high_water: ring.high_water() as u32,
                dropped: ring.dropped().saturating_add(self.down_refused.get()),
            }
        };
        let host_dropped = host[0].dropped.saturating_add(host[1].dropped);
        let status = self.pair.borrow().status(now_ms(), host, down, self.spi.get(), host_dropped);
        self.push_host(&status);
    }
}

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Read before anything else writes it: this is the previous life's marker.
    let cause = reset_cause();
    let last_phase = boot_phase(cause);
    mark(LoopPhase::Boot);
    phase::claim();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    let boot = Instant::now();

    let mut own_mac = [0; 6];
    own_mac.copy_from_slice(esp_hal::efuse::base_mac_address().as_bytes());

    // SDIO D2 and D3 are unused; both ends leave them as inputs.
    let _d2 = Input::new(peripherals.GPIO16, InputConfig::default());
    let _d3 = Input::new(peripherals.GPIO17, InputConfig::default());
    let ready = Input::new(peripherals.GPIO6, InputConfig::default().with_pull(Pull::Down));

    // Hold the C6 in reset, then release it, so the pair starts together.
    let mut c6_en = Output::new(peripherals.GPIO54, Level::Low, OutputConfig::default());
    Timer::after(C6_RESET_HOLD).await;
    c6_en.set_high();

    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default().with_frequency(Rate::from_khz(SPI_KHZ)).with_mode(Mode::_0),
    )
    .expect("SPI2")
    .with_sck(peripherals.GPIO18)
    .with_mosi(peripherals.GPIO19)
    .with_miso(peripherals.GPIO14)
    .with_cs(peripherals.GPIO15)
    .with_dma(peripherals.DMA_AXI_CH0)
    .into_async();
    let bus = Bus { spi, ready, rx: dma_buf!(DmaRxBuf), tx: dma_buf!(DmaTxBuf) };

    let fw_version = ShortStr::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default();
    let gps = cfg!(feature = "gps");
    let pair =
        Pair::new(own_mac, fw_version, cause, last_phase, boot.as_millis(), gps, RADIO_DIGEST);
    let shared = Shared::new(pair, HOST_QUEUE.take(), DOWN_QUEUE.take());
    let mut link = Link::new(boot, own_mac, cause, last_phase, HELD.take());

    // Joined rather than spawned: all borrow `shared`, and none ends.
    let bridge = join3(
        run_link(bus, &mut link, &shared),
        radio_clock(&shared, c6_en),
        usb::run(peripherals.USB_HS, &shared),
    );
    #[cfg(feature = "gps")]
    let pins = gps::Pins { i2c: peripherals.I2C0, sda: peripherals.GPIO7, scl: peripherals.GPIO8 };
    #[cfg(feature = "gps")]
    match join(bridge, gps::run(pins, &shared)).await {}
    #[cfg(not(feature = "gps"))]
    match bridge.await {}
}

/// Give [`Pair`] the time every [`RADIO_TICK`]: announce the life it starts when the C6
/// stays silent, and reset the C6 through `c6_en` when it asks.
async fn radio_clock(shared: &Shared, mut c6_en: Output<'static>) -> ! {
    loop {
        Timer::after(RADIO_TICK).await;
        let event = shared.pair.borrow_mut().on_tick(now_ms());
        match event {
            PairEvent::None => {}
            PairEvent::NewLife => shared.new_life(),
            PairEvent::ResetRadio => {
                c6_en.set_low();
                Timer::after(C6_RESET_HOLD).await;
                c6_en.set_high();
            }
        }
    }
}
