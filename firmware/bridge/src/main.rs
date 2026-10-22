//! wartui's USB ESP-NOW bridge.
//!
//! A laptop cannot speak ESP-NOW, so this dongle does it on the laptop's behalf:
//! it parks a radio on the mesh's control channel, forwards every wartui frame it
//! hears up the USB link, and transmits frames the host hands it.
//!
//! It is deliberately dumb — COBS framing, `esp-radio` and the magic that marks a
//! frame as ours, and nothing else about what the bytes mean. Every rule that
//! could turn out to be wrong lives on the host, where it is unit-testable and a
//! fix costs a `cargo run` rather than a reflash.
//! The interesting property is not that the bridge is simple but that it is
//! *finished*: the parts of this project most likely to change cannot reach it.
//!
//! The one rule that has to be decided here is when the USB transmit endpoint has
//! stopped draining, because by then nothing this end says can reach anybody. Even
//! that is only *applied* here — the arithmetic is
//! [`wartui_proto::stall::StallWatch`], where `cargo test` can reach it, after a
//! version written in this file shipped two defects only a bench found.
//!
//! [`HostToBridge::SendEspNow`] answers with the *transmit-callback* status rather
//! than the enqueue result, which is what lets the host tell a delivered
//! assignment from a hopeful one. Peers are added on demand and removed when the host
//! says so (`RemovePeer`). The radio half is [`wartui_firmware::radio`], shared with every
//! firmware that bridges a radio; sends run there in their own future, so the loop goes on
//! draining the radio while one is in flight. Peer and power commands queue behind sends,
//! so the radio acts in link order.
//!
//! `README.md` has the flashing commands and the `esp-*` dependency set.
#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe with esp-hal types, especially those holding \
    buffers for the duration of a transfer"
)]
#![deny(clippy::large_stack_frames)]

mod config;
#[cfg(feature = "t-dongle-c5-gps")]
mod gps;
#[cfg(any(feature = "t-dongle-c5", feature = "t-dongle-c5-gps"))]
mod led;
#[cfg(feature = "t-dongle-c5")]
mod panel;

use embassy_futures::join::join;
use embassy_futures::select::{Either3, Either4, select3, select4};
use embassy_futures::yield_now;
use embassy_time::Timer;
use esp_hal::Async;
use esp_hal::clock::CpuClock;
#[cfg(feature = "xiao-external-antenna")]
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::time::Instant;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_radio::esp_now::{EspNowManager, EspNowReceiver, ReceivedData};
use esp_radio::wifi::WifiController;
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::phase::{boot_phase, boot_speaks_first, mark};
use wartui_firmware::radio::{
    self, Command, Done, Dones, Full, Heard, Jobs, Payloads, Said, SendDone,
};
use wartui_firmware::{phase, reboot, reset_cause};
#[cfg(feature = "t-dongle-c5-gps")]
use wartui_proto::link::GpsWriteBytes;
use wartui_proto::link::{
    BridgeToHost, Chip, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LinkBurst, LinkError,
    LogLevel, LogStr, LoopPhase, MAX_HOST_FRAME, Panel, PanelLines, QueueId, QueueStat, QueueStats,
    ShortStr, decode_frame,
};
use wartui_proto::mac::Mac;
use wartui_proto::outbox::{ByteSink, Outbox};
use wartui_proto::reset::ResetCause;
use wartui_proto::stall::{HOST_PRESENT_WINDOW_MS, StallWatch};

use crate::config::{RADIO_JOBS, SEND_JOBS};

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6")))]
compile_error!("select a chip: --features esp32c5 or esp32c6");
#[cfg(all(feature = "xiao-external-antenna", not(feature = "esp32c6")))]
compile_error!("xiao-external-antenna drives a XIAO ESP32-C6's RF switch; build it with esp32c6");
#[cfg(all(feature = "t-dongle-c5", not(feature = "esp32c5")))]
compile_error!("t-dongle-c5 is an ESP32-C5 in a USB-A shell; build it with esp32c5");
// Unreachable while the two above hold, since the chips are exclusive too — but
// stated rather than inferred, because what makes these two incompatible is not the
// chip: `xiao-external-antenna` drives GPIO3 and GPIO14, which on a T-Dongle-C5 are
// `LCD_DC` and `USB_DP`.
#[cfg(all(feature = "t-dongle-c5", feature = "xiao-external-antenna"))]
compile_error!("t-dongle-c5 and xiao-external-antenna both drive GPIO3; pick one board");
#[cfg(all(feature = "t-dongle-c5-gps", not(feature = "esp32c5")))]
compile_error!("t-dongle-c5-gps is the T-Dongle-C5's Qwiic port; build it with esp32c5");
// Unreachable for the same reason as the one above, and stated for the same reason: the
// GPS port is a T-Dongle-C5's, and `xiao-external-antenna` drives that board's pins.
#[cfg(all(feature = "t-dongle-c5-gps", feature = "xiao-external-antenna"))]
compile_error!("t-dongle-c5-gps and xiao-external-antenna are different boards; pick one");

/// Counted rather than checked pairwise.
///
/// With two chips `all(a, b)` is the whole of the rule, but it says nothing about
/// a third, and every chip added would need a clause per existing one. Counting
/// cannot rot that way.
///
/// Both this and the `compile_error!` above are backstops that in practice
/// never get to speak: selecting two chips, or none, makes `esp-hal`'s own
/// macros fail first and loudly. They are here to state the rule where someone
/// reading this file will find it, and to keep holding if that ever changes —
/// not because they are the message an operator sees.
const CHIPS_SELECTED: usize =
    cfg!(feature = "esp32c5") as usize + cfg!(feature = "esp32c6") as usize;
const _: () = assert!(CHIPS_SELECTED <= 1, "select exactly one chip feature, not several");

#[cfg(feature = "esp32c5")]
const CHIP: Chip = Chip::Esp32C5;
#[cfg(feature = "esp32c6")]
const CHIP: Chip = Chip::Esp32C6;

/// The screen this board has, announced in [`BridgeToHost::Ready`].
///
/// A board fact rather than a chip fact, which is why it hangs off the board feature
/// and not off `CHIP`. `None` is what removes the operator flag at the other end: a
/// host told there is no panel sends no lines at all.
#[cfg(feature = "t-dongle-c5")]
const PANEL: Option<Panel> = Some(Panel { cols: panel::COLS, rows: panel::ROWS });
#[cfg(not(feature = "t-dongle-c5"))]
const PANEL: Option<Panel> = None;

/// The longest an idle pass waits: one in which neither radio nor link had anything
/// to do.
///
/// A received frame, a host command, a finished send, a finished GPS batch and — while
/// frames are queued for the host — the USB FIFO emptying each end the wait themselves. The
/// FIFO wake is what lets the outbox go on draining after a [`link_burst`] gives the pass
/// back, without a busy loop that burns the core whenever no host is reading.
/// `docs/usb-link-findings.md` has the rates.
///
/// The tick is for work no event announces: [`StallWatch`] judging elapsed time
/// against its three seconds, the panel's redraw floor and its fallback screen, and
/// the [`mark`] breadcrumbs.
///
/// It also defines quiet air for the panel: a wait the tick ends saw no frame, no host
/// byte and no FIFO drain for its whole length, and only the pass after one redraws.
/// A bursting four-node fleet still leaves a silence this long one to two times a
/// second (`docs/t-dongle-c5-findings.md`).
const IDLE_TICK: embassy_time::Duration = embassy_time::Duration::from_millis(100);

/// USB OUT packet size, which is also the receive FIFO's: one read empties it.
const LINK_CHUNK: usize = 64;

/// What ended an idle wait.
enum Woken {
    Frame(ReceivedData),
    Link(usize),
    Drained,
    Done(Done),
    /// The GPS task finished a batch.
    #[cfg(feature = "t-dongle-c5-gps")]
    Gps,
    Tick,
}

/// What an idle wait leaves for `main`: never a [`ReceivedData`], which `main`'s poll
/// frame has no room for.
enum Idled {
    /// The host sent this many bytes into `LINK_BUF`.
    Read(usize),
    /// The tick ended the wait, so the air was quiet for all of it.
    Quiet,
    /// A frame, a finished send, a FIFO drain or a GPS batch ended it. A frame and a send
    /// are already dealt with, and the next pass forwards a batch.
    Busy,
}

// There is no watchdog here, having built one and measured that it cannot work.
// `esp_hal::init` disables every watchdog on the chip
// (`esp-hal-1.2.2/src/lib.rs:768-778`), and the RWDT that would close the gap
// provably never resets these parts — it counts, unfed, and `WDT_PROCPU_RESET_EN`
// is the one enable that will not be written (`docs/phase-3-findings.md` has the
// register dumps, taken on esp-hal 1.1.2).
//
// So a genuine hang in the loop below has nothing behind it and the board has to
// be unplugged: a real gap, left deliberately open, because a safety net that
// provably catches nothing is worse than an absent one — it will be trusted. The
// failure this project has actually seen was never a hang, and [`StallWatch`] is
// what guards it.

/// Resets rather than hanging.
///
/// A halted bridge is invisible: the port stays open and no frames arrive. A reset
/// re-announces [`BridgeToHost::Ready`], so a bridge that panics repeatedly says
/// so in the one way the host is already listening for. The message is lost,
/// which is the price of not printing to the endpoint the link runs over.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    reboot()
}

/// The USB endpoint, as somewhere to put bytes.
///
/// Async for the one thing the main loop awaits — the FIFO emptying — while every
/// byte still goes in through the non-blocking calls, which work in either mode.
struct UsbSink<'d> {
    tx: UsbSerialJtagTx<'d, Async>,
}

impl ByteSink for UsbSink<'_> {
    fn write_byte(&mut self, byte: u8) -> nb::Result<(), ()> {
        self.tx.write_byte_nb(byte).map_err(|err| match err {
            nb::Error::WouldBlock => nb::Error::WouldBlock,
            nb::Error::Other(_) => nb::Error::Other(()),
        })
    }

    // A packet's bytes in one call: one status read and one write per byte, with no
    // per-byte trait call or ring lookup. Halves the cycles a 64-byte fill costs
    // (`docs/link-encode-findings.md`).
    fn write_bytes(&mut self, bytes: &[u8]) -> (usize, nb::Result<(), ()>) {
        for (n, &byte) in bytes.iter().enumerate() {
            if let Err(err) = self.tx.write_byte_nb(byte) {
                let err = match err {
                    nb::Error::WouldBlock => nb::Error::WouldBlock,
                    nb::Error::Other(_) => nb::Error::Other(()),
                };
                return (n, Err(err));
            }
        }
        (bytes.len(), Ok(()))
    }

    fn flush(&mut self) {
        // WouldBlock here means the packet is still in flight, not that the
        // data was lost: `flush_tx_nb` has already set `wr_done`.
        let _ = self.tx.flush_tx_nb();
    }
}

/// The outbound rings, which are thirty-six kilobytes of buffers and belong in
/// `.bss` rather than being built on the stack and moved into place.
static OUTBOX: StaticCell<Outbox> = StaticCell::new();

/// The host-side frame buffer, sized for a full ESP-NOW v2 send. Static for the
/// reason `OUTBOX` is: inside `Bridge` its 1600 bytes are built on the stack twice
/// before `BRIDGE` takes them.
static ACCUMULATOR: StaticCell<FrameAccumulator<MAX_HOST_FRAME>> = StaticCell::new();

/// Where every read from the host lands. Static rather than a local of `main`, whose poll
/// frame sits at `.clippy.toml`'s threshold on the C5.
static LINK_BUF: StaticCell<[u8; LINK_CHUNK]> = StaticCell::new();

/// What [`radio::run_jobs`] works through: the jobs in arrival order, and the payloads of
/// the sends among them.
struct Queues {
    jobs: Jobs<RADIO_JOBS>,
    payloads: Payloads<SEND_JOBS>,
}

/// Static for the reason `LINK_BUF` is, and each payload is a full ESP-NOW v2 one. Built at
/// compile time, so it is never on the stack at all.
static QUEUES: ConstStaticCell<Queues> =
    ConstStaticCell::new(Queues { jobs: Jobs::new(), payloads: Payloads::new() });

/// What became of each job, waiting for the loop to report it.
static DONES: ConstStaticCell<Dones<RADIO_JOBS>> = ConstStaticCell::new(Dones::new());

/// The bridge's state, static for the reason `LINK_BUF` is: as a local of `main` it
/// sits in the poll frame across every `.await`, and once `StallWatch` grew a third
/// timestamp that frame passed `.clippy.toml`'s threshold on the C5.
static BRIDGE: StaticCell<Bridge> = StaticCell::new();

/// Everything that changes while the bridge runs.
struct Bridge {
    outbox: &'static mut Outbox,
    /// Sized for host-to-bridge frames, which carry ESP-NOW v2 sends of up to 1470 bytes.
    accumulator: &'static mut FrameAccumulator<MAX_HOST_FRAME>,
    rx_count: u32,
    /// Received frames too long to be a node's, dropped rather than forwarded.
    rx_oversize: u32,
    /// The most send payloads waiting at once, for `Status.queues`.
    send_jobs_high_water: u32,
    /// Sends refused because the payload pool was full.
    send_jobs_dropped: u32,
    /// The most radio jobs waiting at once.
    radio_jobs_high_water: u32,
    /// Radio jobs refused because their queue was full.
    radio_jobs_dropped: u32,
    boot: Instant,
    /// Why this life started, and where the last one stopped. Fixed at boot
    /// and repeated in every `Ready`, because the host is usually not watching
    /// at the moment a bridge restarts underneath it.
    cause: ResetCause,
    /// Whether this life transmits before a host has spoken, decided once at boot
    /// by [`ResetCause::speaks_first`].
    speaks_first: bool,
    phase: LoopPhase,
    /// Whether the USB transmit endpoint has stopped draining while a host
    /// waited, which is the one failure this firmware recovers from by itself.
    ///
    /// The rule lives in [`wartui_proto::stall`], where `cargo test` reaches it.
    stall: StallWatch,
    /// The lines the host last sent.
    ///
    /// Held here rather than on the screen so that [`handle`] — which borrows the
    /// radio — never has to borrow the panel as well. Empty means no host has said
    /// anything yet, which is what the fallback screen is for.
    #[cfg(feature = "t-dongle-c5")]
    panel_lines: PanelLines,
    /// The screen this bridge actually brought up.
    ///
    /// Starts as [`PANEL`] and is cleared if the panel refuses to start, so a board
    /// whose screen is dead tells the host there is no screen rather than inviting a
    /// line a second at one. What is announced has to be what is true.
    panel: Option<Panel>,
    /// Why there is no screen on a board built to have one.
    ///
    /// Sent with every announcement rather than once. A host is usually not
    /// attached when this firmware starts — that is the whole reason `Identify`
    /// exists — so a fault reported once is a fault nobody ever reads.
    panel_fault: Option<&'static str>,
    /// `main`'s end of the GPS task, when its bus came up.
    ///
    /// Held here rather than in `main` so that [`handle`] can write to it.
    #[cfg(feature = "t-dongle-c5-gps")]
    gps: Option<gps::Link>,
    /// What [`take_gps`] last saw of the receiver, so a change is said once.
    #[cfg(feature = "t-dongle-c5-gps")]
    gps_watch: gps::Watch,
    /// Why no GPS receiver is being read on a board built to have one.
    ///
    /// Sent with every announcement for the reason `panel_fault` is, and cleared when the
    /// receiver answers again.
    gps_fault: Option<&'static str>,
}

impl Bridge {
    /// Microseconds since boot, wrapping after roughly 71 minutes.
    ///
    /// The host only subtracts one of these from another, so the wrap is harmless
    /// and the narrower type keeps the frame small.
    fn now_us(&self) -> u32 {
        self.uptime_us() as u32
    }

    /// Microseconds since boot at full width, which is the clock [`LinkBurst`] runs on.
    fn uptime_us(&self) -> u64 {
        self.boot.elapsed().as_micros()
    }

    /// Milliseconds since boot, which is the clock [`StallWatch`] runs on.
    ///
    /// Full width, unlike [`Bridge::now_us`]: this one is compared against
    /// timeouts, where a wrap would read as a host that spoke in the future.
    fn now_ms(&self) -> u64 {
        self.boot.elapsed().as_millis()
    }

    /// Whether the outbox may write to USB.
    ///
    /// Writing before any host has opened the port wedges the endpoint, so a life that
    /// does not speak first writes nothing, not even its boot terminator, until a host
    /// frame has decoded. `ResetCause::speaks_first` has the bench numbers.
    fn transmit_open(&self) -> bool {
        self.speaks_first || self.stall.last_host().is_some()
    }

    /// Whether a host frame has decoded within [`HOST_PRESENT_WINDOW_MS`], on the
    /// clock [`StallWatch`] already keeps.
    fn host_present(&self) -> bool {
        let now = self.now_ms();
        self.stall.last_host().is_some_and(|at| now - at < HOST_PRESENT_WINDOW_MS)
    }

    /// Queue `msg` for the host, or drop it while transmit is shut.
    ///
    /// Every frame goes through here. While no host has spoken this life, and the life does
    /// not speak first, there is nobody to read it: a backlog held then arrives late when
    /// a host attaches, carries its current position onto old sightings and reads as
    /// missed admin windows. Faults worth keeping are repeated by [`Bridge::announce`].
    /// Nothing dropped here is counted: `dropped_tx` means congestion while a host reads.
    ///
    /// Once a host has spoken, frames queue until the life ends. USB Serial/JTAG gives no
    /// DTR level, so this end cannot see a host leave, and a quiet host such as
    /// `wartui sniff` sends nothing to time it by. What builds after a host quits is at
    /// most a ring's worth, and the next connection's `Identify` clears it
    /// ([`note_host_frame`]).
    fn send(&mut self, msg: &BridgeToHost) {
        if self.transmit_open() {
            self.outbox.send(msg);
        }
    }

    /// Say who we are.
    ///
    /// Sent at boot by a life that [speaks first](ResetCause::speaks_first), and
    /// otherwise when the first host frame decodes. Sent again whenever the host
    /// asks. The host is usually not attached at boot, and this is how it learns the
    /// chip, MAC and link revision.
    fn announce(&mut self, mac: Mac) {
        self.send(&BridgeToHost::Ready {
            chip: CHIP,
            mac,
            fw_version: ShortStr::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
            proto_version: LINK_PROTO_VERSION,
            reset_cause: self.cause,
            last_phase: self.phase,
            // Saturating rather than wrapping: the heaps total 100 KiB so the cast
            // cannot lose anything, but an `as` that silently could is not worth
            // leaving in a frame the host draws conclusions from.
            heap_free: u32::try_from(esp_alloc::HEAP.free()).unwrap_or(u32::MAX),
            // The host's only way to tell this from a second answer to an
            // `Identify`: a software reset keeps the USB device, so both arrive
            // on the same connection.
            uptime_ms: self.boot.elapsed().as_millis() as u32,
            panel: self.panel,
            gps: cfg!(feature = "t-dongle-c5-gps"),
            // The radio is this chip.
            radio: None,
            // It takes no update over its link: `flash-bridge` flashes it with espflash.
            image: None,
        });
        if let Some(why) = self.panel_fault {
            self.error(why);
        }
        if let Some(why) = self.gps_fault {
            self.error(why);
        }
    }

    /// Take the lines the host wants shown.
    ///
    /// Stored rather than drawn: the loop decides when pixels move, so a chatty host
    /// cannot turn a frame into a blocking SPI transfer inside the receive path.
    #[cfg(feature = "t-dongle-c5")]
    fn show_panel(&mut self, lines: &PanelLines) {
        self.panel_lines.clone_from(lines);
    }

    /// Nowhere to draw them. Dropped rather than refused, because a host that has
    /// ignored a `panel: None` must not be able to wedge a bridge that has no screen.
    #[cfg(not(feature = "t-dongle-c5"))]
    #[expect(clippy::unused_self, reason = "the panel build takes `&mut self`")]
    fn show_panel(&self, _lines: &PanelLines) {}

    /// Hand the host's bytes to the GPS task, which writes them. [`take_gps`] reports a
    /// write the receiver refused.
    #[cfg(feature = "t-dongle-c5-gps")]
    fn write_gps(&mut self, bytes: &GpsWriteBytes) {
        if let Some(link) = self.gps.as_mut() {
            link.write(bytes);
        }
    }

    /// No receiver to write to. Dropped for the reason `show_panel` drops lines.
    #[cfg(not(feature = "t-dongle-c5-gps"))]
    #[expect(clippy::unused_self, reason = "the GPS build takes `&mut self`")]
    fn write_gps(&self, _bytes: &[u8]) {}

    fn log(&mut self, level: LogLevel, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.send(&BridgeToHost::Log { level, message });
    }

    fn error(&mut self, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.send(&BridgeToHost::Error { message });
    }

    /// Pass on what a radio call had to say.
    fn say(&mut self, said: Said) {
        match said {
            Said::Nothing => {}
            Said::Log(level, message) => self.log(level, message),
            Said::Error(message) => self.error(message),
        }
    }

    /// Report what became of one radio job.
    fn report(&mut self, done: &Done) {
        match done {
            Done::Sent(sent) => self.report_send(sent),
            &Done::Said(said) => self.say(said),
        }
    }

    /// Report what became of one send.
    ///
    /// `tx_us` is on this bridge's clock: when `radio::run_jobs` saw the transmit callback,
    /// which can trail it (see `SendDone::done_at`), or when a full queue refused the job.
    fn report_send(&mut self, done: &SendDone) {
        let tx_us = (done.done_at - self.boot).as_micros() as u32;
        self.send(&BridgeToHost::SendResult { id: done.id, status: done.status, tx_us });
    }
}

#[esp_rtos::main]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Read before anything else writes it: this is the *previous* life's marker,
    // and `esp_hal::init` has already latched the reset reason to read it against.
    let cause = reset_cause();
    // The T-Dongle-C5's LED, dark as early as it can be: the sooner, the shorter it shows at
    // boot. Held for the life of `main` so the clock never floats.
    #[cfg(any(feature = "t-dongle-c5", feature = "t-dongle-c5-gps"))]
    let _led = led::darken(peripherals.GPIO4, peripherals.GPIO5);
    let phase = boot_phase(cause);
    let speaks_first = boot_speaks_first(cause);
    mark(LoopPhase::Boot);
    phase::claim();

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let (usb_rx, usb_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();
    let mut sink = UsbSink { tx: usb_tx };
    let mut usb_rx = usb_rx;

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // A XIAO ESP32-C6's RF switch, powered, given 100 ms, then set to the U.FL
    // connector before the radio starts and held for the life of `main`.
    // `firmware/node/src/main.rs` has why.
    #[cfg(feature = "xiao-external-antenna")]
    let _antenna = {
        let power = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
        Timer::after_millis(100).await;
        (power, Output::new(peripherals.GPIO14, Level::High, OutputConfig::default()))
    };

    // `esp-radio`'s default is China under `WIFI_COUNTRY_POLICY_MANUAL`, which
    // refuses 5 GHz 100-144 outright. The bridge only ever sits on `CONTROL_CHANNEL`
    // and would never notice, but a C5 node's 5 GHz sweep would lose 100-144 under
    // that default, so the nodes pass `US` and the bridge passes it too: two halves
    // of one fleet disagreeing about what is legal is a trap.
    let mut controller = WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");
    // Split rather than kept whole: the sender goes to `radio::run_jobs`, so a transmit
    // never borrows the receive path. The controller goes there too, for `SetTxPower`: the
    // ESP-NOW handles do not borrow it.
    let (manager, mut sender, receiver) = controller.esp_now().split();
    // Before the first frame. Its failures are reported behind `Ready`, once the outbox
    // exists to carry them.
    let setup = radio::setup(&mut controller, &manager);

    let now = Instant::now();
    let bridge = BRIDGE.init_with(|| Bridge {
        outbox: OUTBOX.init_with(Outbox::new),
        accumulator: ACCUMULATOR.init_with(FrameAccumulator::new),
        rx_count: 0,
        rx_oversize: 0,
        send_jobs_high_water: 0,
        send_jobs_dropped: 0,
        radio_jobs_high_water: 0,
        radio_jobs_dropped: 0,
        boot: now,
        cause,
        speaks_first,
        phase,
        stall: StallWatch::new(),
        #[cfg(feature = "t-dongle-c5")]
        panel_lines: PanelLines::new(),
        panel: PANEL,
        panel_fault: None,
        #[cfg(feature = "t-dongle-c5-gps")]
        gps: None,
        #[cfg(feature = "t-dongle-c5-gps")]
        gps_watch: gps::Watch::new(),
        gps_fault: None,
    });
    // Ahead of every frame this life sends, whenever transmit opens. The ROM banner
    // a reset prints has no `0x00` in it and overflows the host's frame buffer, so
    // without a terminator of our own it runs into the first frame — `Ready` — and
    // the host drops both.
    bridge.outbox.delimit();

    // After the radio, so a panel that refuses its init sequence cannot stop a bridge
    // from bridging, and before `announce`, so the screen is lit by the time anything
    // could be looking at it.
    #[cfg(feature = "t-dongle-c5")]
    let screen = match panel::Screen::new(panel::Pins {
        spi: peripherals.SPI2,
        mosi: peripherals.GPIO2,
        sck: peripherals.GPIO6,
        cs: peripherals.GPIO10,
        dc: peripherals.GPIO3,
        rst: peripherals.GPIO1,
        backlight: peripherals.GPIO0,
        sd_cs: peripherals.GPIO23,
    }) {
        Ok(screen) => Some(screen),
        Err(why) => {
            bridge.panel = None;
            bridge.panel_fault = Some(why);
            None
        }
    };

    // After the radio for the reason the panel is. The receiver is not probed here: the
    // task's first poll does that, and `take_gps` reports a receiver that does not answer.
    #[cfg(feature = "t-dongle-c5-gps")]
    match gps::start(
        gps::Pins { i2c: peripherals.I2C0, sda: peripherals.GPIO11, scl: peripherals.GPIO12 },
        peripherals.FROM_CPU_INTR1,
    ) {
        Ok(link) => bridge.gps = Some(link),
        Err(why) => bridge.gps_fault = Some(why),
    }

    let mac = esp_radio::wifi::Interface::station().mac_address();
    // A life that does not speak first announces when a host does; see `take_link_byte`.
    if bridge.speaks_first {
        bridge.announce(mac);
    }
    for said in setup.said {
        bridge.say(said);
    }

    let queues: &Queues = QUEUES.take();
    let dones = DONES.take();
    let looped = main_loop(
        &mut usb_rx,
        &mut sink,
        &manager,
        receiver,
        #[cfg(feature = "t-dongle-c5")]
        screen,
        bridge,
        mac,
        queues,
        dones,
    );
    // Joined rather than spawned: both halves borrow the ESP-NOW handles `main` owns, and
    // neither ends.
    let ran = radio::run_jobs(
        &mut controller,
        &manager,
        &mut sender,
        setup.tx_power,
        &queues.jobs,
        &queues.payloads,
        dones,
    );
    match join(looped, ran).await {}
}

/// Everything the bridge does but transmit, forever: drain the radio and the link, report
/// finished jobs, and wait when there is nothing to do.
// Each argument is a handle `main` holds, and bundling them would put a struct in its
// poll frame.
#[allow(clippy::too_many_arguments)]
async fn main_loop(
    usb_rx: &mut UsbSerialJtagRx<'_, Async>,
    sink: &mut UsbSink<'_>,
    manager: &EspNowManager,
    mut receiver: EspNowReceiver,
    #[cfg(feature = "t-dongle-c5")] mut screen: Option<&mut panel::Screen>,
    bridge: &mut Bridge,
    mac: Mac,
    queues: &Queues,
    dones: &Dones<RADIO_JOBS>,
) -> ! {
    let link_buf = LINK_BUF.init_with(|| [0; LINK_CHUNK]);
    // True only on the pass straight after a wait the tick ended.
    #[cfg(feature = "t-dongle-c5")]
    let mut quiet = false;
    // True only on the pass straight after a wait the host's bytes ended, which may
    // have left a host frame part-way in.
    let mut link_woke = false;
    loop {
        let mut worked = false;
        mark(LoopPhase::DrainRadio);
        worked |= drain_radio(&receiver, bridge);
        worked |= take_results(dones, bridge);
        let link = link_burst(usb_rx, link_buf, sink, manager, queues, bridge, mac, link_woke);
        worked |= link.moved;
        link_woke = false;

        // Never waits: the GPS task reads the receiver, and this only forwards what it
        // finished.
        #[cfg(feature = "t-dongle-c5-gps")]
        {
            worked |= take_gps(bridge);
        }

        // Named as its own phase because it is a call that can block for longer than a
        // memcpy, so a reset with nobody watching says it was inside it. `render` returns
        // without touching the bus unless a line changed and its floor has passed, so an
        // idle bridge still sleeps.
        #[cfg(feature = "t-dongle-c5")]
        {
            if let Some(screen) = screen.as_mut() {
                // A repaint holds the loop for up to 36 ms, so a job queued above starts
                // first. Only `quiet` passes can repaint, which keeps this off busy ones.
                if quiet && !queues.jobs.is_empty() {
                    yield_now().await;
                }
                mark(LoopPhase::Render);
                worked |= screen.render(bridge, mac, quiet);
            }
            quiet = false;
        }

        // Nothing subtler is available: the endpoint cannot be re-armed from this
        // end, and every way of telling the host goes out through the broken path.
        // So the reset *is* the message — the link drops, comes back, and the
        // `Ready` behind it says `TxStalled`.
        //
        // Nothing is refused while held, so `queued` is false then: the watch must not
        // see a refusal that is really a closed gate, and an armed flush on an empty
        // FIFO busy-spins in `idle_wait`.
        let queued = bridge.transmit_open() && !bridge.outbox.is_empty();
        if bridge.stall.note_tx(link.wrote_since_read, queued, bridge.now_ms()) {
            mark(LoopPhase::TxStalled);
            reboot();
        }

        // Recorded every pass for the next life, whatever ends this one.
        phase::note_host_present(bridge.host_present());

        if !worked {
            mark(LoopPhase::Idle);
            let idled =
                idle_wait(&mut receiver, usb_rx, link_buf, &mut sink.tx, queued, dones, bridge)
                    .await;
            #[cfg(feature = "t-dongle-c5")]
            {
                quiet = matches!(idled, Idled::Quiet);
            }
            if let Idled::Read(read) = idled {
                mark(LoopPhase::DrainLink);
                take_link_bytes(&link_buf[..read], manager, queues, bridge, mac);
                link_woke = true;
            }
        } else if !queues.jobs.is_empty() {
            // A busy pass awaits nothing else, and `radio::run_jobs` runs only while this
            // future is pending. A waiting job needs that poll; yielding on every pass
            // regardless costs drain time under a burst.
            yield_now().await;
        }
    }
}

/// Wait for a received frame, a host command, the FIFO emptying, a finished job, a
/// finished GPS batch or the tick, and say which it was.
///
/// A frame or a finished job that ends the wait is reported here and the host's bytes
/// are handed back, each as soon as it is taken: it is older than anything the next pass's
/// drains find behind it, so order holds. Both halves live outside `main_loop`'s poll
/// frame, which sits at `.clippy.toml`'s threshold on the C5 and has no room for a
/// [`ReceivedData`].
///
/// Every arm is cancel-safe: the radio's queue, the USB FIFO and the job results give up
/// an item only in the poll that returns it, the flush holds no bytes, and the GPS channel
/// gives up a batch only at `receive_done`, which only [`take_gps`] calls.
///
/// A batch the GPS task finishes while this waits ends it, so a batch reaches the outbox
/// within one pass of being finished rather than a tick.
async fn idle_wait(
    receiver: &mut EspNowReceiver,
    usb_rx: &mut UsbSerialJtagRx<'_, Async>,
    link_buf: &mut [u8; LINK_CHUNK],
    usb_tx: &mut UsbSerialJtagTx<'_, Async>,
    queued: bool,
    dones: &Dones<RADIO_JOBS>,
    bridge: &mut Bridge,
) -> Idled {
    let radio = async { Woken::Frame(receiver.receive_async().await) };
    let link = async {
        let read = embedded_io_async::Read::read(usb_rx, link_buf).await;
        Woken::Link(read.unwrap_or(0))
    };
    let flushed = async {
        if queued {
            // The flush's `Result` is ignored because it cannot carry news: it is
            // always `Ok`, and a FIFO that never empties is the stall watch's to
            // judge, from `moved` on the passes that follow.
            let _ = embedded_io_async::Write::flush(usb_tx).await;
        } else {
            core::future::pending::<()>().await;
        }
        Woken::Drained
    };
    #[cfg(feature = "t-dongle-c5-gps")]
    let batches = bridge.gps.as_mut().map(gps::Link::batches);
    let gps = async {
        #[cfg(feature = "t-dongle-c5-gps")]
        if let Some(batches) = batches {
            batches.receive().await;
            return Woken::Gps;
        }
        core::future::pending::<Woken>().await
    };
    let sent = async { Woken::Done(dones.receive().await) };
    let woken =
        match select4(radio, link, flushed, select3(sent, gps, Timer::after(IDLE_TICK))).await {
            Either4::First(woken)
            | Either4::Second(woken)
            | Either4::Third(woken)
            | Either4::Fourth(Either3::First(woken) | Either3::Second(woken)) => woken,
            Either4::Fourth(Either3::Third(())) => Woken::Tick,
        };
    // The read arms the RX-packet interrupt and the flush the TX-empty one, and
    // neither has a `Drop` to disarm it, so whichever lost would later wake this
    // task for nothing.
    let usb = esp_hal::peripherals::USB_DEVICE::regs();
    usb.int_ena().modify(|_, w| w.serial_in_empty().clear_bit().serial_out_recv_pkt().clear_bit());
    usb.int_clr()
        .write(|w| w.serial_in_empty().clear_bit_by_one().serial_out_recv_pkt().clear_bit_by_one());
    match woken {
        Woken::Frame(received) => {
            mark(LoopPhase::DrainRadio);
            forward(&received, bridge);
            Idled::Busy
        }
        // An error reads nothing, which is no evidence of quiet air.
        Woken::Link(read) if read > 0 => Idled::Read(read),
        Woken::Done(done) => {
            bridge.report(&done);
            Idled::Busy
        }
        Woken::Link(_) | Woken::Drained => Idled::Busy,
        #[cfg(feature = "t-dongle-c5-gps")]
        Woken::Gps => Idled::Busy,
        Woken::Tick => Idled::Quiet,
    }
}

/// Move every wartui frame the radio has heard into the outbox.
fn drain_radio(receiver: &EspNowReceiver, bridge: &mut Bridge) -> bool {
    let mut worked = false;

    while let Some(received) = receiver.receive() {
        worked = true;
        forward(&received, bridge);
    }

    worked
}

/// Queue one received frame for the host, if it is ours.
///
/// Out of line because it builds a 1048-byte [`BridgeToHost`], and `main`'s poll frame
/// sits near `.clippy.toml`'s threshold on the C5. `radio::hear` has what is ours.
#[inline(never)]
fn forward(received: &ReceivedData, bridge: &mut Bridge) {
    let rx_us = bridge.now_us();
    match radio::hear(received, rx_us, |rx| bridge.send(rx)) {
        Heard::Forwarded => bridge.rx_count = bridge.rx_count.wrapping_add(1),
        // Counted apart from `rx_count`, which the host reads as frames headed its way.
        Heard::Oversize => bridge.rx_oversize = bridge.rx_oversize.wrapping_add(1),
        Heard::Foreign => {}
    }
}

/// Report every job [`radio::run_jobs`] has finished. Says whether there was one.
fn take_results(dones: &Dones<RADIO_JOBS>, bridge: &mut Bridge) -> bool {
    let mut worked = false;
    while let Ok(done) = dones.try_receive() {
        worked = true;
        bridge.report(&done);
    }
    worked
}

/// What the bridge reports when the GPS receiver stops answering.
#[cfg(feature = "t-dongle-c5-gps")]
const NO_GPS_RECEIVER: &str = "no GPS receiver answered on the Qwiic port";

/// Queue every batch the GPS task has finished, and report what changed about the
/// receiver. Says whether a batch was taken. Never waits on the task.
///
/// A batch is taken while transmit is shut too, and dropped, so the task never backs up.
///
/// A change in whether it answers is said once, not at every probe: an `Error` when it
/// stops, which `announce` then repeats, and a `Log` when it comes back.
///
/// Out of line for the reason [`forward`] is.
#[cfg(feature = "t-dongle-c5-gps")]
#[inline(never)]
fn take_gps(bridge: &mut Bridge) -> bool {
    // The gate `Bridge::send` applies, read once: `link` borrows the bridge.
    let open = bridge.transmit_open();
    let Some(link) = bridge.gps.as_mut() else { return false };
    let mut taken = false;
    while let Some(bytes) = link.batches().try_receive() {
        if open {
            bridge.outbox.send(&BridgeToHost::Gps { bytes: bytes.clone() });
        }
        link.batches().receive_done();
        taken = true;
    }
    let changes = bridge.gps_watch.changes();
    match changes.answered {
        Some(true) => {
            bridge.gps_fault = None;
            bridge.log(LogLevel::Info, "the GPS receiver answered on the Qwiic port");
        }
        Some(false) => {
            bridge.gps_fault = Some(NO_GPS_RECEIVER);
            bridge.error(NO_GPS_RECEIVER);
        }
        None => {}
    }
    if changes.write_refused {
        bridge.error("the GPS receiver refused a write");
    }
    taken
}

/// What one pass's [`link_burst`] moved.
struct LinkMoved {
    /// Whether any byte moved, in either direction.
    moved: bool,
    /// Whether bytes were written towards the host at or after the last poll that read
    /// host bytes. This is the pass's `moved` for [`StallWatch`].
    wrote_since_read: bool,
}

/// Read and pump the link, and stay on it while a frame is part-way across, in either
/// direction.
///
/// One read empties the OUT FIFO, which holds one packet ([`LINK_CHUNK`]), and the IN
/// FIFO holds one packet too. A pass that moves one and goes on leaves the rest of the
/// frame a packet per pass: the host's next packet is NAKed until the OUT FIFO empties,
/// and the next IN packet waits on the FIFO-empty wake. So after the first poll this
/// polls both back to back until [`LinkBurst`] gives the pass back.
/// `crates/wartui-proto/src/link/burst.rs` has why, and the bounds.
///
/// The first poll always runs. The polls after it run only once link bytes have moved,
/// in that poll or in the wait before this pass (`link_woke`). Otherwise no frame is
/// crossing, or its host has stopped, and spinning buys nothing.
///
/// The polling is bounded by `BURST_BUDGET_US`, which keeps a host that never stops
/// writing out of the radio's receive queue. A frame a poll completes is acted on inside
/// the burst and adds its own time. A radio command adds only its hand-off to
/// [`radio::run_jobs`]. Neither is a wait on the host, which is what neither firmware may
/// do to the USB endpoint.
///
/// [`StallWatch`] sees `wrote_since_read`. Host bytes read prove a host, not a draining
/// endpoint, so only bytes written count. And a write counts only if no later poll read
/// host bytes: a request read after the last write is unanswered, and reporting the
/// earlier write would clear the watch's record of it. A poll reads before it pumps, so
/// its own write counts. A burst against a host that has stopped reading writes nothing
/// and ends after `BURST_GAP_US`.
///
/// Out of line for the reason [`forward`] is, and to keep its state out of `main`'s
/// poll frame.
#[inline(never)]
// Each argument is a handle `main` holds, and bundling them would put a struct in its
// poll frame.
#[allow(clippy::too_many_arguments)]
fn link_burst(
    usb_rx: &mut UsbSerialJtagRx<'_, Async>,
    link_buf: &mut [u8; LINK_CHUNK],
    sink: &mut UsbSink<'_>,
    manager: &EspNowManager,
    queues: &Queues,
    bridge: &mut Bridge,
    mac: Mac,
    link_woke: bool,
) -> LinkMoved {
    let mut pass = LinkMoved { moved: false, wrote_since_read: false };
    // Begun after the first poll, and only if that poll or the wait before it moved bytes.
    let mut burst: Option<LinkBurst> = None;
    loop {
        mark(LoopPhase::DrainLink);
        let read = usb_rx.drain_rx_fifo(link_buf);
        take_link_bytes(&link_buf[..read], manager, queues, bridge, mac);

        // Asked again: the bytes just read may hold the first host frame of this life.
        let wrote = bridge.transmit_open() && {
            mark(LoopPhase::Pump);
            bridge.outbox.pump(sink)
        };

        let moved = read > 0 || wrote;
        pass.moved |= moved;
        if wrote {
            pass.wrote_since_read = true;
        } else if read > 0 {
            pass.wrote_since_read = false;
        }

        let now = bridge.uptime_us();
        if burst.is_none() && !moved && !link_woke {
            return pass;
        }
        let run = burst.get_or_insert(LinkBurst::start(now));
        let mid_frame =
            !bridge.accumulator.is_empty() || (bridge.transmit_open() && bridge.outbox.in_flight());
        if !run.keep_going(now, moved, mid_frame) {
            return pass;
        }
    }
}

/// Feed bytes from the host to the accumulator, acting on each frame they complete.
fn take_link_bytes(
    bytes: &[u8],
    manager: &EspNowManager,
    queues: &Queues,
    bridge: &mut Bridge,
    mac: Mac,
) {
    for &byte in bytes {
        take_link_byte(byte, manager, queues, bridge, mac);
    }
}

/// Feed one byte from the host to the accumulator, and act on the frame it completes.
///
/// Out of line for the reason [`forward`] is.
#[inline(never)]
fn take_link_byte(
    byte: u8,
    manager: &EspNowManager,
    queues: &Queues,
    bridge: &mut Bridge,
    mac: Mac,
) {
    let Some(frame) = bridge.accumulator.push(byte) else { return };
    match decode_frame::<HostToBridge>(frame) {
        Ok(command) => {
            // Proof of a host, and only a frame that decoded counts: a board
            // running node firmware talks constantly and none of it is a
            // frame, which `StallWatch` must not read as somebody waiting. Whether
            // this one asks for a reply decides if it can start the stall clock.
            note_host_frame(
                bridge,
                mac,
                command.asks_for_reply(),
                matches!(command, HostToBridge::Identify),
            );
            handle(&command, manager, queues, bridge, mac);
        }
        Err(err) => {
            // A frame that passed framing and its checksum but carries another
            // protocol version is still a host writing, since only a host writes to
            // this input. It opens transmit, and the `Ready` it earns carries our
            // version so the host can report the mismatch. It asks for nothing this
            // end can answer.
            if matches!(err, LinkError::VersionMismatch { .. }) {
                note_host_frame(bridge, mac, false, false);
            }
            // Expected after a reset, when the ROM banner arrives down the
            // same pipe. At debug, so a real version mismatch still shows.
            let mut message = LogStr::new();
            let _ = core::fmt::Write::write_fmt(&mut message, format_args!("{err}"));
            bridge.send(&BridgeToHost::Log { level: LogLevel::Debug, message });
        }
    }
}

/// Record proof of a host, opening transmit if this is the first.
///
/// The first frame of a life opens transmit for one that does not speak first.
/// Anything but `Identify` still earns a `Ready` ahead of its own answer, so a host
/// that was already running can tell this is a new life. `asks` is
/// [`HostToBridge::asks_for_reply`], which decides whether the frame can start the
/// stall clock.
///
/// An `Identify` empties the outbox before it is answered: each connection sends exactly
/// one, first, so whatever is queued then was for a host that has gone. A first frame of
/// any other kind finds nothing to clear, since [`Bridge::send`] drops everything until
/// it. The presence this frame records goes ahead of the `Ready`, which
/// [`Bridge::send`] would drop without it.
fn note_host_frame(bridge: &mut Bridge, mac: Mac, asks: bool, identify: bool) {
    let first = bridge.stall.last_host().is_none();
    if identify {
        bridge.outbox.clear();
    }
    bridge.stall.note_host(bridge.now_ms(), asks);
    if first && !bridge.speaks_first && !identify {
        bridge.announce(mac);
    }
}

/// Carry out one host command.
///
/// By reference: a `HostToBridge` is 1488 bytes, and a by-value parameter is a second
/// copy in this frame on top of the one `take_link_byte` decoded.
fn handle(
    command: &HostToBridge,
    manager: &EspNowManager,
    queues: &Queues,
    bridge: &mut Bridge,
    mac: Mac,
) {
    mark(LoopPhase::Command);
    match command {
        HostToBridge::Identify => bridge.announce(mac),

        HostToBridge::GetStatus => {
            let peer_count = manager
                .peer_count()
                .map(|count| count.total_count.clamp(0, u8::MAX as i32) as u8)
                .unwrap_or(0);
            let uptime_ms = bridge.boot.elapsed().as_millis() as u32;
            bridge.send(&BridgeToHost::Status {
                peer_count,
                rx_count: bridge.rx_count,
                dropped_tx: bridge.outbox.dropped(),
                uptime_ms,
                rx_oversize: bridge.rx_oversize,
                queues: send_queue_stats(bridge),
                // This bridge has no SPI link.
                spi: None,
            });
        }

        HostToBridge::Reset => reboot(),

        HostToBridge::ShowPanel { lines } => bridge.show_panel(lines),

        HostToBridge::GpsWrite { bytes } => bridge.write_gps(bytes),

        // Only a P4 bridge updates over its link; the host asks this one by mistake.
        HostToBridge::Update(_) => bridge.error("this bridge takes no update over its link"),

        HostToBridge::SetTxPower { .. }
        | HostToBridge::SendEspNow { .. }
        | HostToBridge::AddPeer { .. }
        | HostToBridge::RemovePeer { .. } => queue_command(command, queues, bridge),
    }
}

/// Hand a radio command to [`radio::run_jobs`], which keeps the order commands arrive in
/// and whose result a later pass reports. A full queue is answered now, and counted.
///
/// Out of line for the reason [`forward`] is: a send's 1470-byte payload is copied into the
/// pool, on top of the `HostToBridge` the caller decoded.
#[inline(never)]
fn queue_command(command: &HostToBridge, queues: &Queues, bridge: &mut Bridge) {
    let command = match *command {
        HostToBridge::SetTxPower { power } => Command::SetTxPower(power),
        HostToBridge::AddPeer { mac } => Command::AddPeer(mac),
        HostToBridge::RemovePeer { mac } => Command::RemovePeer(mac),
        HostToBridge::SendEspNow { id, dst, ensure_peer, ref payload } => {
            let queued =
                radio::queue_send(&queues.jobs, &queues.payloads, id, dst, ensure_peer, payload);
            return note_queued(queued, queues, bridge);
        }
        _ => return,
    };
    let queued = radio::queue_command(&queues.jobs, command).map(|done| (Full::Jobs, done));
    note_queued(queued, queues, bridge);
}

/// Report a refusal and count it against the queue that was full, and note both queues'
/// depths.
fn note_queued(refused: Option<(Full, Done)>, queues: &Queues, bridge: &mut Bridge) {
    if let Some((full, done)) = refused {
        match full {
            Full::Jobs => bridge.radio_jobs_dropped = bridge.radio_jobs_dropped.wrapping_add(1),
            Full::Payloads => bridge.send_jobs_dropped = bridge.send_jobs_dropped.wrapping_add(1),
        }
        bridge.report(&done);
    }
    let jobs = u32::try_from(queues.jobs.len()).unwrap_or(u32::MAX);
    bridge.radio_jobs_high_water = bridge.radio_jobs_high_water.max(jobs);
    let payloads = u32::try_from(queues.payloads.len()).unwrap_or(u32::MAX);
    bridge.send_jobs_high_water = bridge.send_jobs_high_water.max(payloads);
}

/// The send payload pool's and the radio-job queue's depths, for `Status.queues`.
fn send_queue_stats(bridge: &Bridge) -> QueueStats {
    let mut queues = QueueStats::new();
    let _ = queues.push(QueueStat {
        queue: QueueId::RadioSendJobs,
        capacity: SEND_JOBS as u32,
        high_water: bridge.send_jobs_high_water,
        dropped: bridge.send_jobs_dropped,
    });
    let _ = queues.push(QueueStat {
        queue: QueueId::RadioJobs,
        capacity: RADIO_JOBS as u32,
        high_water: bridge.radio_jobs_high_water,
        dropped: bridge.radio_jobs_dropped,
    });
    queues
}
