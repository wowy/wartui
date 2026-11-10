//! The host half of wartui's P4 split bridge, on a Waveshare ESP32-P4-WIFI6.
//!
//! The P4 talks to the host over UART0, which the board's CH343 carries to the Type-C, with
//! wartui's link protocol, through UHCI's DMA. It talks to the board's ESP32-C6 over UART1 with
//! hardware flow control, on the FIFO; the C6 holds that link for a future Bluetooth host link
//! (`firmware/p4-radio`). The host sees one bridge, reporting both chips'
//! diagnostics; [`wartui_proto::split::Pair`] makes the decisions that keep it so.
//!
//! The futures share one task through `join`, so they share [`Shared`] through `Cell` and
//! `RefCell` with no locking and nothing borrowed across an `.await`:
//!
//! - **the UART link** (`src/link.rs`) runs from boot, so the C6's hello arrives, and sends
//!   the C6 the image this P4 carries (`src/radio.rs`) when it runs another;
//! - **the radio clock** sleeps until [`Pair::next_deadline_ms`], or until the pair gains a
//!   deadline, then gives [`Pair`] the time. That is how it gives up on a silent C6, resets one
//!   silent since an update's install through its enable pin, and reports one silent since a
//!   host's poll;
//! - **the host link** (`src/host.rs`) runs a reader, a writer, the gate that tells whether a
//!   host is present, and the `GetStatus` answer. The reader also takes the P4's own updates
//!   (`src/update.rs`), queues the host's records for its nodes, and fills the node image cache
//!   in PSRAM that their update chunks are built from (`src/cache.rs`);
//! - **the GPS reader** (`src/gps.rs`) polls a u-blox receiver on I2C0 and forwards its
//!   bytes. GPIO7 and GPIO8 are shared with the board's ES8311 codec, which this firmware
//!   never addresses;
//! - **the carrier** (`src/carrier.rs`) powers a carrier board's nodes one at a time at boot,
//!   then power-cycles one whenever the host asks, and staggers them again after a lost node
//!   supply, through the I/O expander that shares I2C0 with the GPS. Each locks the bus per
//!   transaction;
//! - **the node buses** (`src/bus.rs`), one future per SPI controller, carry each node's records
//!   to and from the host and the fleet planner, and report link counters to the host as log
//!   lines;
//! - **the fan** (`src/fan.rs`), on a carrier only, drives the lid fan's PWM at the duty stored on
//!   flash, which a host's `SetFan` changes, and reads its tach for `Status`;
//! - **the fleet planner** (`src/fleet.rs`) sleeps until its core's next deadline, or until an
//!   input wakes it, then ticks the core and sends what it owes. It decides what every node
//!   scans, reading their heartbeats, and authors every `Assign` and `Stop`. It boots stopped, and
//!   scans from a host's `StartScan` until a `StopScan`, whether or not a host is listening.
//!
//! The settings (`src/settings.rs`) are read from flash once at boot, on every board: the fan's
//! duty and the channel pool, which `Status` reports. A host's `SetFan` or `SetPool` stores its
//! change there. The scan mode is not stored.
//!
//! Before anything else, the P4 checks whether its own image is on trial, and rolls it back
//! when its trial boots are spent (`wartui_firmware::ota`). A trial boot arms the RTC
//! watchdog, and the first host frame once the C6 has said hello confirms the image
//! (`src/update.rs`).
//!
//! The P4 resets the C6 through its enable pin at boot, so the pair always starts
//! together. It reads the carrier's expander before it builds the node buses, since the board
//! picks their CS pins. Every wait is a DMA completion, a UART event, a GPIO
//! level, a signal between the futures or a timer.
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

mod cache;
mod carrier;
mod config;
mod fan;
mod fleet;
mod gps;
mod host;
mod link;
mod radio;
mod settings;
mod update;

use core::cell::{Cell, RefCell};

use embassy_futures::join::{join, join4};
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_sync::zerocopy_channel::Sender;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuf, DmaTxBuf, descriptor_count};
use esp_hal::gpio::{AnyPin, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::I2c;
use esp_hal::peripherals::{GPIO7, GPIO8, I2C0};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::uhci::{self, Uhci};
use esp_hal::uart::{
    BaudrateTolerance, Config as UartConfig, CtsConfig, HwFlowControl, RtsConfig,
    RxConfig as UartRxConfig, Uart,
};
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::phase::{boot_phase, mark};
use wartui_firmware::{gps as gps_bus, phase, reboot, reset_cause};
use wartui_proto::bus::PER_BUS;
use wartui_proto::fleet::Fleet;
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::host_queue::{HOST_CHUNK, HostQueue, is_priority};
use wartui_proto::link::{
    BridgeToHost, HOST_BAUD, LogStr, LoopPhase, MAX_FRAME, QueueId, QueueStat, RadioLinkStats,
    ShortStr, encode_frame,
};
use wartui_proto::link::{GpsBytes, GpsWriteBytes};
use wartui_proto::ota::SECTOR;
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::radio_link::{
    BAUD_RATE, BODY_MAX, Down, FIFO_FULL_THRESHOLD, Held, PacketReader, RECORD_MAX, RTS_THRESHOLD,
    WIRE_MAX, encode_down,
};
use wartui_proto::split::{P4Counters, Pair, PairEvent};

use crate::cache::Cache;
use crate::carrier::{Carrier, Power};
use crate::config::{DOWN_QUEUE_BYTES, HOST_BULK_BYTES, HOST_PRIORITY_BYTES};
use crate::host::Presence;
use crate::link::{Bus, Link, run_link};
use crate::radio::RADIO_DIGEST;
use crate::update::Updates;

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

/// How long the C6 is held in reset, at boot and when the pair asks.
const C6_RESET_HOLD: Duration = Duration::from_millis(50);

type HostFrames = HostQueue<HOST_PRIORITY_BYTES, HOST_BULK_BYTES>;
type DownQueue = FrameRing<DOWN_QUEUE_BYTES>;

/// Frames waiting for the host. Built at compile time, so the rings are never on the stack.
static HOST_QUEUE: ConstStaticCell<HostFrames> = ConstStaticCell::new(HostQueue::new());
/// `Down` records waiting for the C6.
static DOWN_QUEUE: ConstStaticCell<DownQueue> = ConstStaticCell::new(FrameRing::new());
/// The record that did not fit the last packet.
static HELD: ConstStaticCell<Held> = ConstStaticCell::new(Held::new());
/// The packet the reader is part-way through.
static READER: ConstStaticCell<PacketReader> = ConstStaticCell::new(PacketReader::new());
/// Where the writer builds a packet's body.
static BODY: ConstStaticCell<[u8; BODY_MAX]> = ConstStaticCell::new([0; BODY_MAX]);
/// Where the writer encodes the packet for the wire.
static WIRE: ConstStaticCell<[u8; WIRE_MAX]> = ConstStaticCell::new([0; WIRE_MAX]);

/// The fleet planner's core, stopped and cutting the default pool until `Shared::new` gives it the
/// stored one. Built at compile time, so its slot table is never on the stack.
static FLEET: ConstStaticCell<Fleet> =
    ConstStaticCell::new(Fleet::new(wartui_proto::ChannelPool::All));

/// What the futures share. Static, to keep it out of `main`'s future, which `.clippy.toml`'s
/// stack threshold bounds.
static SHARED: StaticCell<Shared> = StaticCell::new();

/// I2C0, shared by the GPS reader and the carrier's expander. Both lock it per transaction, and
/// every future is in one task, so the lock needs no more than [`NoopRawMutex`].
pub type I2cBus = Mutex<NoopRawMutex, I2c<'static, Async>>;
static I2C0_BUS: StaticCell<I2cBus> = StaticCell::new();

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

// After `dma_buf!`, which it uses.
mod bus;

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
    radio_link: Cell<RadioLinkStats>,
    /// Raised by whatever may have given the link's writer something to send, so a waiting
    /// writer looks again: a queued record, a hello owed, an update status, a change in the pair.
    wake_link: Signal<NoopRawMutex, ()>,
    /// Whether a host is present, by the frames it sends (`src/host.rs`).
    presence: Presence,
    /// Raised when a host frame opens the gate.
    host_arrived: Signal<NoopRawMutex, ()>,
    /// Line errors on the host UART since boot.
    host_uart_errors: Cell<u32>,
    /// The host asked for a `Status`.
    status_asked: Signal<NoopRawMutex, ()>,
    /// The C6's counters arrived.
    radio_status: Signal<NoopRawMutex, ()>,
    /// The pair gained a deadline, so the radio clock wakes to wait for it.
    deadline_set: Signal<NoopRawMutex, ()>,
    /// Why no GPS receiver is being read. Sent with every announcement, because a host
    /// usually attaches after boot, and cleared when the receiver answers again.
    gps_fault: Cell<Option<&'static str>>,
    /// The GPS reader's write channel, once its bus is up.
    gps_writes: RefCell<Option<Sender<'static, NoopRawMutex, GpsWriteBytes>>>,
    /// The P4's own image: its trial, and an update in progress.
    updates: RefCell<&'static mut Updates>,
    /// The node image the host uploaded, which update chunks are built from.
    cache: RefCell<Cache>,
    /// The node buses' slots, as the host reader and both buses see them.
    nodes: bus::Nodes,
    /// The carrier's node power, as the host reader and the carrier future see it.
    power: Power,
    /// The carrier's lid fan, or `None` with no carrier.
    fan: Option<&'static fan::Fan>,
    /// The settings sector's image, read before each write (`src/settings.rs`).
    sector: RefCell<&'static mut [u8; SECTOR]>,
    /// The fleet planner's core: the mode, the pool in force and the Bluetooth pick, which
    /// `Status` reports, and every slot's share (`src/fleet.rs`).
    fleet: RefCell<&'static mut Fleet>,
    /// Raised by an input that [`fleet::run`] must pump after, or whose deadline it must see.
    fleet_wake: Signal<NoopRawMutex, ()>,
    /// The core deadline [`fleet::run`] sleeps toward, or `None` while it waits for a wake alone.
    fleet_sleep: Cell<Option<u64>>,
}

impl Shared {
    fn new(
        pair: Pair,
        host: &'static mut HostFrames,
        down: &'static mut DownQueue,
        updates: &'static mut Updates,
        cache: Cache,
        carrier: &Carrier,
    ) -> Self {
        // The stored settings, read before any future runs.
        let (sector, stored) = settings::boot(updates);
        let fan = carrier.board().is_carrier().then(|| fan::Fan::boot(stored.fan));
        let fleet = FLEET.take();
        fleet.set_pool(stored.pool.unwrap_or_default(), now_ms());
        Self {
            // Here rather than in `main`, whose frame another `Pair` would grow.
            pair: RefCell::new(pair.with_node_board(carrier.board())),
            host: RefCell::new(host),
            pushed: Signal::new(),
            down: RefCell::new(down),
            down_refused: Cell::new(0),
            radio_link: Cell::new(RadioLinkStats {
                packets: 0,
                crc_fail: 0,
                bad_magic: 0,
                seq_gap: 0,
                uart_errors: 0,
                radio_crc_fail: 0,
                radio_bad_magic: 0,
                radio_seq_gap: 0,
                radio_uart_errors: 0,
            }),
            wake_link: Signal::new(),
            presence: Presence::new(),
            host_arrived: Signal::new(),
            host_uart_errors: Cell::new(0),
            status_asked: Signal::new(),
            radio_status: Signal::new(),
            deadline_set: Signal::new(),
            gps_fault: Cell::new(None),
            gps_writes: RefCell::new(None),
            updates: RefCell::new(updates),
            cache: RefCell::new(cache),
            nodes: bus::Nodes::new(carrier.per_bus()),
            power: Power::new(carrier),
            fan,
            sector: RefCell::new(sector),
            fleet: RefCell::new(fleet),
            fleet_wake: Signal::new(),
            fleet_sleep: Cell::new(None),
        }
    }

    /// Add one to the link counter `field` picks.
    fn count_link(&self, field: impl FnOnce(&mut RadioLinkStats) -> &mut u32) {
        let mut radio_link = self.radio_link.get();
        let counter = field(&mut radio_link);
        *counter = counter.wrapping_add(1);
        self.radio_link.set(radio_link);
    }

    /// Queue `msg` for the host. A full ring evicts its oldest frame and counts the drop.
    ///
    /// Dropped, and not counted, while no host is present. A backlog held for an absent host
    /// arrives late when one attaches, and the host stamps its current position onto the old
    /// sightings in it. Faults worth keeping are repeated by
    /// [`Shared::announce`], and the queue is cleared when a host arrives (`src/host.rs`).
    ///
    /// Out of line, with the encode buffer, to keep both out of the callers' frames.
    #[inline(never)]
    fn push_host(&self, msg: &BridgeToHost) {
        if !self.presence.is_open() {
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
    /// Refused rather than evicting the oldest: an evicted record is lost with nothing to say
    /// so.
    #[inline(never)]
    fn push_down(&self, down: &Down) -> bool {
        let mut record = [0; RECORD_MAX];
        let mut ring = self.down.borrow_mut();
        // Each record sits behind the ring's two-byte length.
        let fits = |len: usize| DownQueue::CAPACITY - ring.len_bytes() >= 2 + len;
        let queued = match encode_down(down, &mut record) {
            Ok(len) if fits(len) => ring.push(&record[..len]),
            _ => false,
        };
        if queued {
            self.wake_link.signal(());
        } else {
            self.down_refused.set(self.down_refused.get().wrapping_add(1));
        }
        queued
    }

    /// Announce this bridge life: `Ready`, then the fault if the radio is not usable, the
    /// GPS fault if there is one, what the carrier and the node image cache have to say, and what
    /// the P4's own image has to say.
    ///
    /// Nothing while the pair is still waiting for the C6's first hello, since there is no
    /// `Ready` to lead with yet and the host drops what arrives before one. The life that ends
    /// the wait announces itself (`Shared::new_life`).
    fn announce(&self) {
        if !self.push_ready() {
            return;
        }
        self.push_fault();
        if let Some(why) = self.gps_fault.get() {
            self.push_error(why);
        }
        carrier::say(self);
        cache::say(self);
        fleet::say(self);
        self.flush_update_lines();
    }

    /// Send the lines the P4's image held for the host, then why it refuses updates, if it
    /// does. Only after a `Ready`, since the host drops what arrives on a connection before
    /// one, and only to a host that is present, since the lines are said once.
    fn flush_update_lines(&self) {
        if !self.presence.is_open() {
            return;
        }
        update::say_lines(self);
        if let Some(message) = update::fault(self) {
            self.push_error(&message);
        }
    }

    /// Queue `Ready`, returning whether the pair had one to give.
    ///
    /// Out of line, as each message is, so the two never share a frame.
    #[inline(never)]
    fn push_ready(&self) -> bool {
        let ready = self.pair.borrow().ready(now_ms());
        let Some(ready) = ready else { return false };
        self.push_host(&ready);
        true
    }

    #[inline(never)]
    fn push_fault(&self) {
        let fault = self.pair.borrow().fault();
        if let Some(message) = fault {
            self.push_host(&BridgeToHost::Error { message });
        }
    }

    #[inline(never)]
    fn push_error(&self, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.push_host(&BridgeToHost::Error { message });
    }

    #[inline(never)]
    fn push_log(&self, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.push_host(&BridgeToHost::Log { message });
    }

    /// Queue a batch from the GPS receiver, copied out of its slot here rather than in the
    /// reader's future.
    #[inline(never)]
    fn push_gps(&self, bytes: &GpsBytes) {
        self.push_host(&BridgeToHost::Gps { bytes: bytes.clone() });
    }

    /// A new pair life began: tell a host that is present, and the link's writer, since the
    /// pair may bridge now.
    fn new_life(&self) {
        self.wake_link.signal(());
        self.announce();
    }

    /// Queue the `Status` a poll asked for, then the plan, so a host always holds the current one.
    fn push_status(&self) {
        self.push_status_only();
        fleet::say(self);
    }

    #[inline(never)]
    fn push_status_only(&self) {
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
        let (rx_count, rx_oversize) = self.nodes.rx_counts();
        let now = now_ms();
        let fleet = self.fleet.borrow();
        let counters = P4Counters {
            host,
            down,
            node_down: self.nodes.stat(),
            radio_link: self.radio_link.get(),
            host_dropped: host[0].dropped.saturating_add(host[1].dropped),
            host_uart_errors: self.host_uart_errors.get(),
            rx_count,
            rx_oversize,
            fan: self.fan.map(fan::Fan::stats),
            pool: fleet.pool(),
            scan: fleet.mode(),
            ble: fleet.bluetooth(),
            present: self.nodes.present(),
            // Every slot the planner can assign, so the host can name the slot of each batch.
            members: fleet.members(now),
        };
        drop(fleet);
        let status = self.pair.borrow().status(now, counters);
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

    // Before anything else, so an image that has run out of trial boots is rolled back before
    // it does anything. What it says waits for a host.
    let (updates, own_image) = update::boot(peripherals.FLASH, peripherals.RTC_TIMER);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    // The PSRAM, for the node image cache (`src/cache.rs`), mapped before the buses start.
    let cache = Cache::new(peripherals.PSRAM);
    let boot = Instant::now();

    let mut own_mac = [0; 6];
    own_mac.copy_from_slice(esp_hal::efuse::base_mac_address().as_bytes());

    // SDIO D2 and D3 are unused; both ends leave them as inputs.
    let _d2 = Input::new(peripherals.GPIO16, InputConfig::default());
    let _d3 = Input::new(peripherals.GPIO17, InputConfig::default());
    // A spare trace, kept from floating.
    let _spare = Input::new(peripherals.GPIO6, InputConfig::default().with_pull(Pull::Down));

    // Hold the C6 in reset, then release it, so the pair starts together.
    let mut c6_en = Output::new(peripherals.GPIO54, Level::Low, OutputConfig::default());
    Timer::after(C6_RESET_HOLD).await;
    c6_en.set_high();

    // The C6 link. Refused rather than run slightly off, since the C6 is not.
    let c6_uart = Uart::new(
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
    .with_tx(peripherals.GPIO19)
    .with_rx(peripherals.GPIO14)
    .with_rts(peripherals.GPIO18)
    .with_cts(peripherals.GPIO15)
    .into_async();
    let (rx, tx) = c6_uart.split();
    let bus = Bus { rx, tx, reader: READER.take(), body: BODY.take(), wire: WIRE.take() };

    // The host link, on the CH343's pins. Exact, as the C6 link is: the P4 divides 4 Mbaud
    // exactly from its crystal.
    let host_uart = Uart::new(
        peripherals.UART0,
        UartConfig::default()
            .with_baudrate(HOST_BAUD)
            .with_baudrate_tolerance(BaudrateTolerance::Exact),
    )
    .expect("UART0")
    .with_tx(peripherals.GPIO37)
    .with_rx(peripherals.GPIO38);
    let mut uhci = Uhci::new(host_uart, peripherals.UHCI0, peripherals.DMA_CH0);
    uhci.apply_rx_config(&uhci::RxConfig::default().with_chunk_limit(host::RX_LEN as u16))
        .expect("UHCI read limit");
    // Despite its name, this config sets when a read ends: at the limit, or on an idle line.
    // `Uhci::new` clears both.
    uhci.apply_tx_config(&uhci::TxConfig::default()).expect("UHCI end of read");
    let (rx, tx) = uhci.into_async().split();
    let host_bus = host::Bus {
        rx,
        tx,
        rx_bufs: [dma_buf!(DmaRxBuf, host::RX_LEN), dma_buf!(DmaRxBuf, host::RX_LEN)],
        tx_buf: dma_buf!(DmaTxBuf, HOST_CHUNK),
    };

    let (i2c, carrier) = i2c0(peripherals.I2C0, peripherals.GPIO7, peripherals.GPIO8);

    // The node buses (`docs/node-bus.md`), slot 1 first. Only the 14-node carrier wires slot 7,
    // so only it takes GPIO25 and GPIO24 from the USB-Serial-JTAG. The board's slots get their
    // pins, and the rest stay unconfigured.
    let mut cs_a: [Option<AnyPin<'static>>; PER_BUS as usize] = [
        Some(peripherals.GPIO26.into()),
        Some(peripherals.GPIO27.into()),
        Some(peripherals.GPIO32.into()),
        Some(peripherals.GPIO33.into()),
        Some(peripherals.GPIO46.into()),
        Some(peripherals.GPIO47.into()),
        None,
    ];
    let mut cs_b: [Option<AnyPin<'static>>; PER_BUS as usize] = [
        Some(peripherals.GPIO50.into()),
        Some(peripherals.GPIO49.into()),
        Some(peripherals.GPIO5.into()),
        Some(peripherals.GPIO4.into()),
        Some(peripherals.GPIO3.into()),
        Some(peripherals.GPIO2.into()),
        None,
    ];
    let per_bus = usize::from(carrier.per_bus());
    if per_bus == usize::from(PER_BUS) {
        cs_a[6] = Some(peripherals.GPIO25.into());
        cs_b[6] = Some(peripherals.GPIO24.into());
    }
    for cs in cs_a[per_bus..].iter_mut().chain(&mut cs_b[per_bus..]) {
        *cs = None;
    }
    let pins_a = bus::Pins {
        sck: peripherals.GPIO20.into(),
        mosi: peripherals.GPIO21.into(),
        miso: peripherals.GPIO22.into(),
        irq: peripherals.GPIO23.into(),
        cs: cs_a,
    };
    let pins_b = bus::Pins {
        sck: peripherals.GPIO31.into(),
        mosi: peripherals.GPIO30.into(),
        miso: peripherals.GPIO29.into(),
        irq: peripherals.GPIO28.into(),
        cs: cs_b,
    };
    let (bus_a, bus_b) = bus::build(
        carrier.board(),
        carrier.per_bus(),
        (peripherals.SPI2, peripherals.DMA_AXI_CH0, pins_a),
        (peripherals.SPI3, peripherals.DMA_AXI_CH1, pins_b),
    );

    let fan_pins =
        fan::Pins { rmt: peripherals.RMT, pwm: peripherals.GPIO51, tach: peripherals.GPIO52 };

    let fw_version = ShortStr::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default();
    let pair = Pair::new(own_mac, fw_version, cause, last_phase, boot.as_millis(), RADIO_DIGEST)
        .with_own_image(own_image);
    let shared = SHARED.init_with(|| {
        Shared::new(pair, HOST_QUEUE.take(), DOWN_QUEUE.take(), updates, cache, &carrier)
    });
    let link_image = own_image.unwrap_or([0; DIGEST_LEN]);
    let link = RefCell::new(Link::new(boot, own_mac, cause, last_phase, HELD.take(), link_image));

    // Joined rather than spawned: all borrow `shared`, and none ends.
    match join(
        join4(
            run_link(bus, &link, shared),
            radio_clock(shared, c6_en),
            host::run(host_bus, shared),
            gps::run(i2c, shared),
        ),
        join4(
            carrier::run(carrier, i2c.ok(), shared),
            bus::run(bus_a, shared),
            bus::run(bus_b, shared),
            join(fan::run(fan_pins, shared), fleet::run(shared)),
        ),
    )
    .await {}
}

/// Bring up I2C0 for the GPS receiver and a carrier's expander, read the expander blocking, then
/// share the bus. A bus that refuses its configuration is the GPS fault, and no carrier read.
///
/// Out of line, so the drivers it moves are on this frame rather than `main`'s.
#[inline(never)]
fn i2c0(
    i2c: I2C0<'static>,
    sda: GPIO7<'static>,
    scl: GPIO8<'static>,
) -> (Result<&'static I2cBus, &'static str>, Carrier) {
    match gps_bus::bus(i2c, sda, scl) {
        Ok(mut i2c) => {
            let carrier = carrier::detect(&mut i2c);
            let shared: &'static I2cBus = I2C0_BUS.init(Mutex::new(i2c.into_async()));
            (Ok(shared), carrier)
        }
        Err(why) => (Err(why), Carrier::unread()),
    }
}

/// Give [`Pair`] the time at its next deadline: announce the life it starts when the C6
/// stays silent, reset the C6 through `c6_en` when it asks, and tell the host when a poll went
/// unanswered.
async fn radio_clock(shared: &Shared, mut c6_en: Output<'static>) -> ! {
    loop {
        let deadline = shared.pair.borrow().next_deadline_ms();
        match deadline {
            Some(ms) => {
                select(Timer::at(Instant::from_millis(ms)), shared.deadline_set.wait()).await;
            }
            None => shared.deadline_set.wait().await,
        }
        let event = shared.pair.borrow_mut().on_tick(now_ms());
        match event {
            PairEvent::None => {}
            PairEvent::NewLife => shared.new_life(),
            PairEvent::ResetRadio => {
                c6_en.set_low();
                Timer::after(C6_RESET_HOLD).await;
                c6_en.set_high();
            }
            PairEvent::RadioSilent => shared.push_fault(),
        }
    }
}
