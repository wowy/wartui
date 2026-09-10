//! wartui's USB ESP-NOW bridge.
//!
//! A laptop cannot speak ESP-NOW, so this dongle does it on the laptop's
//! behalf: it parks a radio on the mesh's control channel, forwards every frame
//! it hears up the USB link, and — from Phase 4 — transmits frames the host
//! hands it.
//!
//! It is deliberately dumb. It understands COBS framing and it understands
//! `esp-radio`, and nothing whatsoever about what the bytes mean: not the
//! `ENOW` header, not heartbeats, not channel assignments. Every rule that
//! could turn out to be wrong lives on the host, where it is unit-testable and
//! a fix costs a `cargo run` rather than a reflash. The interesting property is
//! not that the bridge is simple, it is that the bridge is *finished* — the
//! parts of this project most likely to change cannot reach it.
//!
//! The one rule that has to be decided here rather than on the host is when the
//! USB transmit endpoint has stopped draining, because by then nothing this end
//! says can reach anybody. Even that is only *applied* here: the arithmetic is
//! [`wartui_proto::stall::StallWatch`], where `cargo test` can reach it. It was
//! written in this file once and shipped two defects that only a bench found.
//!
//! It transmits. [`HostToBridge::SendEspNow`] hands the payload straight to
//! the radio and answers with the *transmit-callback* status rather than the
//! enqueue result, which is the one thing the vendor core gets wrong
//! (`src/WiFiOps.cpp:679`) and the reason wartui can tell a delivered
//! assignment from a hopeful one. Peers are added on demand and never removed
//! as a side effect of sending.
//!
//! ## Flashing
//!
//! ```bash
//! cargo run --release --features esp32c6   # or --features esp32c5
//! cargo +esp run --release --features esp32s3 --target xtensa-esp32s3-none-elf
//! ```
//!
//! The runner is `espflash flash --monitor` with no `--chip`, so it detects the
//! part itself. Note that `--monitor` prints the framed link bytes as text and
//! will look like noise; use `wartui sniff` instead.
//!
//! The S3 is the odd one out because it is Xtensa: it needs `espup`'s `esp`
//! toolchain and a `core` built from source, neither of which the RISC-V parts
//! want. The `+esp` override beats `rust-toolchain.toml`, which stays on
//! stable so that a C5 or a C6 costs nobody a second toolchain.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe with esp-hal types, especially those holding \
    buffers for the duration of a transfer"
)]
#![deny(clippy::large_stack_frames)]

use esp_hal::Blocking;
use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rtc_cntl::SocResetReason;
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_radio::esp_now::{
    EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface, PeerInfo,
};
use esp_rtos::CurrentThreadHandle;
use portable_atomic::{AtomicU8, AtomicU32, Ordering};
use static_cell::StaticCell;
use wartui_proto::heapless::Vec;
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LogLevel,
    LogStr, LoopPhase, MAX_FRAME, Mac, ResetCause, SendStatus, ShortStr, decode_frame,
};
use wartui_proto::outbox::{ByteSink, Outbox};
/// Where the stock mesh lives (`src/WiFiOps.cpp:15`). Shared with the node
/// firmware and the host planner rather than spelled again here: nothing on the
/// air negotiates this number, so the only thing keeping the three ends on the
/// same channel is that they read it from the same place. The host can move
/// this bridge with [`HostToBridge::SetChannel`], but nothing else will follow.
use wartui_proto::plan::CONTROL_CHANNEL as DEFAULT_CHANNEL;
use wartui_proto::stall::StallWatch;

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6", feature = "esp32s3")))]
compile_error!("select a chip: --features esp32c5, esp32c6 or esp32s3");

/// Counted rather than checked pairwise.
///
/// With two chips `all(a, b)` was the whole of the rule; with three it says
/// nothing about `a + c` or `b + c`, and the next chip would need three more
/// clauses. Counting cannot rot that way.
///
/// Both this and the `compile_error!` above are backstops that in practice
/// never get to speak: selecting two chips, or none, makes `esp-hal`'s own
/// macros fail first and loudly. They are here to state the rule where someone
/// reading this file will find it, and to keep holding if that ever changes —
/// not because they are the message an operator sees.
const CHIPS_SELECTED: usize = cfg!(feature = "esp32c5") as usize
    + cfg!(feature = "esp32c6") as usize
    + cfg!(feature = "esp32s3") as usize;
const _: () = assert!(CHIPS_SELECTED <= 1, "select exactly one chip feature, not several");

#[cfg(feature = "esp32c5")]
const CHIP: Chip = Chip::Esp32C5;
#[cfg(feature = "esp32c6")]
const CHIP: Chip = Chip::Esp32C6;
#[cfg(feature = "esp32s3")]
const CHIP: Chip = Chip::Esp32S3;

/// Bytes to take from the USB endpoint in one pass.
///
/// Bounded so a host that dumps a burst of commands cannot keep us out of the
/// radio's receive queue, which is ten frames deep and drops its oldest.
const USB_READ_BUDGET: usize = 256;

/// How long to idle when neither radio nor link had anything to do.
///
/// A busy loop would work — the scheduler is preemptive, so the Wi-Fi task
/// still runs — but it would burn the core for nothing. One millisecond is
/// three hundred times finer than the 300 ms admin window that any of this has
/// to hit.
const IDLE_SLEEP: Duration = Duration::from_millis(1);

// There is no watchdog here, having built one and measured that it does not
// work. `esp_hal::init` disables every watchdog on the chip
// (`esp-hal-1.1.2/src/lib.rs:751-761`), so a genuine hang in the loop below has
// nothing behind it and the board has to be unplugged — a real gap, left
// deliberately open. The RWDT that would close it never fires on these parts
// with esp-hal 1.1.2: enabled at five seconds, tried both before `esp_hal::init`
// and after the radio was up, with `enable()` called first so that its wholesale
// write of `wdtconfig0` could not undo the rest, a build spinning in `loop {}`
// was never reset in any arrangement. Nothing in `esp-rtos` or `esp-radio`
// mentions the RWDT, so nothing is feeding it; the remaining suspicion is the
// C6's LP_WDT clock not being ungated by `Rtc::new`, which would send every one
// of those register writes nowhere — measured false: the registers read back
// exactly as configured, seconds later. It counts, unfed, and its reset never
// reaches the CPU. `WDT_PROCPU_RESET_EN` is the one enable that will not be
// written. `docs/phase-3-findings.md` has the register dumps.
//
// A safety net that provably catches nothing is worse than an absent one,
// because it will be trusted. The failure this project has actually seen was
// never a hang, and [`StallWatch`] is what guards it.

/// Where the loop was when it last stopped making progress.
///
/// In RTC fast memory and marked persistent, so it survives the resets that
/// matter — the panic handler's, the watchdog's, and the one [`StallWatch`]
/// asks for — and is read back by the next life. A power-on leaves it
/// undefined, which is why [`boot_phase`] only believes it when the reset
/// reason says the RTC domain was not reset.
///
/// An `AtomicU8` rather than a `u8` because `unsafe_code` is forbidden here
/// and a plain `static mut` cannot be written without it. It is never
/// contended: only the main task touches it.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static PHASE: AtomicU8 = AtomicU8::new(0);

/// Says that [`PHASE`] was written by a build that writes it.
///
/// Persistent RTC memory holds whatever the last thing to use it left there,
/// which after a reflash is the previous firmware's data and after a power-on
/// is nothing in particular. Neither is a phase, and both would be reported as
/// one with a straight face — the first board flashed with this firmware
/// claimed it had stopped at `Boot`, which was a bit pattern rather than an
/// observation. A word that this build alone writes is what separates a marker
/// from a coincidence.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static PHASE_VALID: AtomicU32 = AtomicU32::new(0);

/// Arbitrary, and only has to be unlikely. Reads as `war1` in a memory dump.
const PHASE_MAGIC: u32 = 0x7761_7231;

/// Record where the loop is, for the next life to report.
fn mark(phase: LoopPhase) {
    PHASE.store(phase as u8, Ordering::Relaxed);
}

/// What the marker said, if this reset is one that preserved it.
///
/// A power-on gives uninitialised RTC memory, and reporting whatever bit
/// pattern it held would put a confident and invented phase in front of an
/// operator. Anything else reached us through a reset that left the RTC domain
/// alone, so the marker is real.
fn boot_phase(cause: ResetCause) -> LoopPhase {
    if matches!(cause, ResetCause::PowerOn) || PHASE_VALID.load(Ordering::Relaxed) != PHASE_MAGIC {
        return LoopPhase::Unknown;
    }
    match PHASE.load(Ordering::Relaxed) {
        1 => LoopPhase::Boot,
        2 => LoopPhase::DrainRadio,
        3 => LoopPhase::DrainLink,
        4 => LoopPhase::Command,
        5 => LoopPhase::Transmit,
        6 => LoopPhase::Pump,
        7 => LoopPhase::Idle,
        8 => LoopPhase::TxStalled,
        _ => LoopPhase::Unknown,
    }
}

/// Flatten the chip's reset reason into the handful of stories worth telling.
///
/// `SocResetReason` names silicon blocks rather than causes, and the three
/// chips disagree about both which blocks exist and what to call them. Matching
/// only the variants every chip defines was the first version of this and it
/// silently cost the C5 its two: `PowerGlitch` and `CpuLockup` exist on that
/// part alone and fell through to [`ResetCause::Unknown`], which on a chip
/// nothing has ever been bench-tested on is the worst place to lose a signal.
///
/// The S3 makes the same trap worse, because its differences are *renames*
/// rather than additions and so they are compile errors rather than silence:
/// the CPU-scoped variants the RISC-V parts spell `Cpu0Sw`, `Cpu0Mwdt0`,
/// `Cpu0Mwdt1` and `Cpu0RtcWdt` are `CpuSw`, `CpuMwdt0`, `CpuMwdt1` and
/// `CpuRtcWdt` there, for the good reason that the S3 has two cores and neither
/// is privileged. The *numeric* codes behind those names are identical on all
/// three parts — 0x0C is a software CPU reset everywhere — which is exactly why
/// this is worth a comment: nothing about a build failing here means the chips
/// actually behave differently, and the temptation to match on `reason as u8`
/// and be done should be resisted, because the enum is the only thing that
/// makes the next chip's differences visible at all.
///
/// What is left unmapped is deliberate. `CoreDeepSleep` cannot happen: nothing
/// here sleeps. `CoreSDIO` (C6) and `CoreEfuseCrc` (all three) are real but say
/// nothing an operator could act on beyond "this board is unwell", which
/// [`ResetCause::Unknown`] already says.
fn reset_cause() -> ResetCause {
    let Some(reason) = esp_hal::system::reset_reason() else {
        return ResetCause::Unknown;
    };
    match reason {
        SocResetReason::ChipPowerOn => ResetCause::PowerOn,
        // Both the panic handler and `HostToBridge::Reset` arrive here, and so
        // does the reset `StallWatch` asks for. Which of the three it was is
        // what the phase marker is for.
        SocResetReason::CoreSw => ResetCause::Software,
        #[cfg(any(feature = "esp32c5", feature = "esp32c6"))]
        SocResetReason::Cpu0Sw => ResetCause::Software,
        #[cfg(feature = "esp32s3")]
        SocResetReason::CpuSw => ResetCause::Software,
        SocResetReason::CoreMwdt0
        | SocResetReason::CoreMwdt1
        | SocResetReason::CoreRtcWdt
        | SocResetReason::SysRtcWdt
        | SocResetReason::SysSuperWdt => ResetCause::Watchdog,
        #[cfg(any(feature = "esp32c5", feature = "esp32c6"))]
        SocResetReason::Cpu0Mwdt0 | SocResetReason::Cpu0Mwdt1 | SocResetReason::Cpu0RtcWdt => {
            ResetCause::Watchdog
        }
        #[cfg(feature = "esp32s3")]
        SocResetReason::CpuMwdt0 | SocResetReason::CpuMwdt1 | SocResetReason::CpuRtcWdt => {
            ResetCause::Watchdog
        }
        // A glitch on the supply rail is a brownout as far as anyone holding
        // the board is concerned, and the remedy printed for it — check the
        // cable and the hub — is the right one.
        #[cfg(feature = "esp32c5")]
        SocResetReason::PowerGlitch => ResetCause::Brownout,
        #[cfg(feature = "esp32s3")]
        SocResetReason::CorePwrGlitch => ResetCause::Brownout,
        // The S3's *other* glitch detector, which is not the same story told
        // twice: esp-hal names 0x17 "glitch on power" and 0x13 "glitch on
        // clock", and only the first one is answered by a different cable.
        #[cfg(feature = "esp32s3")]
        SocResetReason::SysClkGlitch => ResetCause::ClockGlitch,
        // The only signal any of these parts gives for the hang class, and only
        // the C5 gives it. There is no working watchdog behind it to fall back
        // on, so on a C6 or an S3 the hang class is simply unreported.
        #[cfg(feature = "esp32c5")]
        SocResetReason::CpuLockup => ResetCause::Lockup,
        SocResetReason::SysBrownOut => ResetCause::Brownout,
        // `espflash reset` drives this pair over DTR/RTS, so an operator who
        // reached for the tool sees that they did. The S3 has no `Cpu0JtagCpu`
        // and reports the same act as `CoreUsbJtag`.
        SocResetReason::CoreUsbUart | SocResetReason::CoreUsbJtag => ResetCause::External,
        #[cfg(any(feature = "esp32c5", feature = "esp32c6"))]
        SocResetReason::Cpu0JtagCpu => ResetCause::External,
        _ => ResetCause::Unknown,
    }
}

/// Resets rather than hanging.
///
/// A halted bridge is invisible: the port stays open, no frames arrive, and the
/// operator power-cycles it to find out why. A reset re-announces
/// [`BridgeToHost::Ready`], so a bridge that panics repeatedly says so in the
/// one way the host is already listening for. The panic message is lost, which
/// is the price of not being allowed to print to the USB endpoint the link
/// runs over.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    esp_hal::system::software_reset()
}

/// The USB endpoint, as somewhere to put bytes.
struct UsbSink<'d> {
    tx: UsbSerialJtagTx<'d, Blocking>,
}

impl ByteSink for UsbSink<'_> {
    fn write_byte(&mut self, byte: u8) -> nb::Result<(), ()> {
        self.tx.write_byte_nb(byte).map_err(|err| match err {
            nb::Error::WouldBlock => nb::Error::WouldBlock,
            nb::Error::Other(_) => nb::Error::Other(()),
        })
    }

    fn flush(&mut self) {
        // WouldBlock here means the packet is still in flight, not that the
        // data was lost: `flush_tx_nb` has already set `wr_done`.
        let _ = self.tx.flush_tx_nb();
    }
}

/// The outbound rings, which are sixteen kilobytes of buffers and belong in
/// `.bss` rather than being built on the stack and moved into place.
static OUTBOX: StaticCell<Outbox> = StaticCell::new();

/// Everything that changes while the bridge runs.
struct Bridge {
    outbox: &'static mut Outbox,
    accumulator: FrameAccumulator<MAX_FRAME>,
    channel: u8,
    rx_count: u32,
    boot: Instant,
    /// Why this life started, and where the last one stopped. Fixed at boot
    /// and repeated in every `Ready`, because the host is usually not watching
    /// at the moment a bridge restarts underneath it.
    cause: ResetCause,
    phase: LoopPhase,
    /// Whether the USB transmit endpoint has stopped draining while a host
    /// waited, which is the one failure this firmware recovers from by itself.
    ///
    /// The rule is four lines of arithmetic against a clock and it has been
    /// wrong twice, so it lives in [`wartui_proto::stall`] where `cargo test`
    /// can reach it rather than here where only a bench can.
    stall: StallWatch,
}

impl Bridge {
    /// Microseconds since boot, wrapping after roughly 71 minutes.
    ///
    /// The host only ever subtracts one of these from another to measure a
    /// heartbeat-to-assignment latency, so the wrap is harmless and the
    /// narrower type keeps the frame small.
    fn now_us(&self) -> u32 {
        self.boot.elapsed().as_micros() as u32
    }

    /// Milliseconds since boot, which is the clock [`StallWatch`] runs on.
    ///
    /// Full width, unlike [`Bridge::now_us`]: that one is a latency the host
    /// subtracts from another and may wrap, while this one is compared against
    /// timeouts and a wrap would read as a host that spoke in the future.
    fn now_ms(&self) -> u64 {
        self.boot.elapsed().as_millis()
    }

    /// Say who we are.
    ///
    /// Sent at boot and again whenever the host asks, since the host is usually
    /// not attached at boot and `Ready` is how it learns the chip, the MAC and
    /// which revision of the link this build speaks.
    fn announce(&mut self, mac: Mac) {
        self.outbox.send(&BridgeToHost::Ready {
            chip: CHIP,
            mac,
            fw_version: ShortStr::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
            proto_version: LINK_PROTO_VERSION,
            reset_cause: self.cause,
            last_phase: self.phase,
            // Saturating rather than wrapping: the heaps here total 100 KiB,
            // so the cast cannot lose anything, but a `as` that silently could
            // is not worth leaving in a frame the host draws conclusions from.
            heap_free: u32::try_from(esp_alloc::HEAP.free()).unwrap_or(u32::MAX),
            // The host's only way to tell this frame from a second answer to
            // an `Identify` it sent twice, both of which arrive on the same
            // connection because a software reset keeps the USB device.
            uptime_ms: self.boot.elapsed().as_millis() as u32,
        });
    }

    fn log(&mut self, level: LogLevel, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.outbox.send(&BridgeToHost::Log { level, message });
    }

    fn error(&mut self, message: &str) {
        let message = LogStr::try_from(message).unwrap_or_default();
        self.outbox.send(&BridgeToHost::Error { message });
    }
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // Read before anything else writes it: this is the marker the *previous*
    // life left behind, and `esp_hal::init` has already latched the reset
    // reason it has to be interpreted against.
    let cause = reset_cause();
    let phase = boot_phase(cause);
    mark(LoopPhase::Boot);
    // Claimed only after the previous life's marker has been read, so a reset
    // between the two cannot make the next boot trust a phase nobody wrote.
    PHASE_VALID.store(PHASE_MAGIC, Ordering::Relaxed);

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let (usb_rx, usb_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).split();
    let mut sink = UsbSink { tx: usb_tx };
    let mut usb_rx = usb_rx;

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    // `Default::default()` would be China, which is `esp-radio`'s default and
    // not a neutral one: it is applied under `WIFI_COUNTRY_POLICY_MANUAL`, and
    // it refuses 5 GHz 100-144 outright. The bridge sits on channel 6 and would
    // never notice — but `--channel` is a plain `u8` the operator can point
    // anywhere, and a fleet moved to a channel this domain forbids would stop
    // here while the nodes, which set `US`, were perfectly willing to go. Unlike
    // a node, this end does say so: `SetChannel` answers a refusal with an
    // `Error` frame the host prints. The reason to set it anyway is that two
    // halves of one fleet disagreeing about what is legal is a trap even when
    // one half can describe it.
    let controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");
    // Split rather than kept whole: `EspNowSender::send` needs `&mut`, and
    // holding the manager and receiver separately means a transmit does not
    // have to borrow the parts that answer `GetStatus` and drain the radio.
    let (manager, mut sender, receiver) = controller.esp_now().split();

    let now = Instant::now();
    let mut bridge = Bridge {
        outbox: OUTBOX.init_with(Outbox::new),
        accumulator: FrameAccumulator::new(),
        channel: DEFAULT_CHANNEL,
        rx_count: 0,
        boot: now,
        cause,
        phase,
        stall: StallWatch::new(),
    };

    match manager.set_channel(DEFAULT_CHANNEL) {
        Ok(()) => {}
        Err(_) => bridge.error("could not park the radio on the default channel"),
    }

    let mac = esp_radio::wifi::Interface::station().mac_address();
    bridge.announce(mac);

    loop {
        let mut worked = false;
        mark(LoopPhase::DrainRadio);
        worked |= drain_radio(&receiver, &mut bridge);
        mark(LoopPhase::DrainLink);
        worked |= drain_link(&mut usb_rx, &manager, &mut sender, &mut bridge, mac);

        mark(LoopPhase::Pump);
        let moved = bridge.outbox.pump(&mut sink);
        worked |= moved;

        // Nothing subtler is available. The endpoint cannot be re-armed from
        // this end, and every way of telling the host would go out through the
        // path that is broken — so the reset *is* the message: the host sees
        // the link drop and come back, and the `Ready` behind it says
        // `TxStalled`, which is the whole diagnosis in one frame.
        let queued = !bridge.outbox.is_empty();
        if bridge.stall.note_tx(moved, queued, bridge.now_ms()) {
            mark(LoopPhase::TxStalled);
            esp_hal::system::software_reset();
        }

        if !worked {
            mark(LoopPhase::Idle);
            CurrentThreadHandle::get().delay(IDLE_SLEEP);
        }
    }
}

/// Move everything the radio has heard into the outbox.
fn drain_radio(receiver: &EspNowReceiver<'_>, bridge: &mut Bridge) -> bool {
    let mut worked = false;

    while let Some(received) = receiver.receive() {
        worked = true;
        bridge.rx_count = bridge.rx_count.wrapping_add(1);

        let Ok(payload) = Vec::from_slice(received.data()) else {
            // Longer than ESP-NOW's own 250-byte ceiling, so it cannot have
            // come from the mesh. Say so rather than truncate it.
            bridge.log(LogLevel::Warn, "dropped an over-long ESP-NOW frame");
            continue;
        };

        let rx_us = bridge.now_us();
        bridge.outbox.send(&BridgeToHost::Rx {
            src: received.info.src_address,
            dst: received.info.dst_address,
            // The only receive-control field every supported chip agrees on.
            //
            // It arrives unsigned. `wifi_pkt_rx_ctrl_t.rssi` is a signed 8-bit
            // bitfield, but the generated accessor extracts the bits unsigned
            // and transmutes, so -62 dBm reaches us as 194 and any clamp to
            // `i8` saturates every frame to 127. Reinterpreting the low byte
            // recovers the value, and keeps working unchanged if the binding is
            // ever fixed to sign-extend.
            rssi: (received.info.rx_control.rssi as u8) as i8,
            // Reported from our own state, not the frame: the per-chip
            // receive-control structs do not all carry a channel.
            channel: bridge.channel,
            rx_us,
            payload,
        });
    }

    worked
}

/// Read what the host has sent and act on complete frames.
fn drain_link(
    usb_rx: &mut UsbSerialJtagRx<'_, Blocking>,
    manager: &EspNowManager<'_>,
    sender: &mut EspNowSender<'_>,
    bridge: &mut Bridge,
    mac: Mac,
) -> bool {
    let mut worked = false;

    for _ in 0..USB_READ_BUDGET {
        let Ok(byte) = usb_rx.read_byte() else { break };
        worked = true;

        let Some(frame) = bridge.accumulator.push(byte) else { continue };
        match decode_frame::<HostToBridge>(frame) {
            Ok(command) => {
                // Proof of a host: something on the other end of this cable
                // speaks the link protocol and is asking us for things. Only a
                // frame that decoded counts — a board running node firmware
                // talks constantly and none of it is a frame, and
                // [`StallWatch::note_tx`] must not read that as somebody waiting
                // on an answer.
                bridge.stall.note_host(bridge.now_ms());
                handle(command, manager, sender, bridge, mac);
            }
            Err(err) => {
                // Expected after a reset, when the ROM bootloader's banner
                // arrives down the same pipe. Reported at debug so a genuine
                // version mismatch is still visible without the banner making
                // noise every boot.
                let mut message = LogStr::new();
                let _ = core::fmt::Write::write_fmt(&mut message, format_args!("{err}"));
                bridge.outbox.send(&BridgeToHost::Log { level: LogLevel::Debug, message });
            }
        }
    }

    worked
}

/// Carry out one host command.
fn handle(
    command: HostToBridge,
    manager: &EspNowManager<'_>,
    sender: &mut EspNowSender<'_>,
    bridge: &mut Bridge,
    mac: Mac,
) {
    mark(LoopPhase::Command);
    match command {
        HostToBridge::Identify => bridge.announce(mac),

        HostToBridge::SetChannel { channel } => match manager.set_channel(channel) {
            Ok(()) => {
                bridge.channel = channel;
                bridge.log(LogLevel::Info, "channel changed");
            }
            Err(_) => bridge.error("the radio refused that channel"),
        },

        HostToBridge::GetStatus => {
            let peer_count = manager
                .peer_count()
                .map(|count| count.total_count.clamp(0, u8::MAX as i32) as u8)
                .unwrap_or(0);
            let uptime_ms = bridge.boot.elapsed().as_millis() as u32;
            bridge.outbox.send(&BridgeToHost::Status {
                channel: bridge.channel,
                peer_count,
                rx_count: bridge.rx_count,
                dropped_tx: bridge.outbox.dropped(),
                uptime_ms,
            });
        }

        HostToBridge::Reset => esp_hal::system::software_reset(),

        HostToBridge::SendEspNow { id, dst, ensure_peer, payload } => {
            // Named separately from `Command` because it is the one place the
            // loop can block for an unbounded time: `SendWaiter` busy-waits on
            // a callback with no timeout of its own, so a phase of `Transmit`
            // behind a watchdog reset says which call did not come back.
            mark(LoopPhase::Transmit);
            let status = transmit(manager, sender, &dst, ensure_peer, &payload);
            // Stamped *after* the transmit callback, not before the send.
            // Subtracted from the `rx_us` of the heartbeat that opened the
            // node's admin window, this is the real time from "the node is
            // listening" to "the radio says the node has it" — the number
            // that decides whether a bridge this dumb can hit a 300 ms
            // window, measured rather than argued about.
            let tx_us = bridge.now_us();
            bridge.outbox.send(&BridgeToHost::SendResult { id, status, tx_us });
        }

        HostToBridge::AddPeer { mac } => match manager.add_peer(peer(&mac)) {
            Ok(()) => bridge.log(LogLevel::Debug, "peer added"),
            // Already known is the outcome the host wanted, not a failure.
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
            Err(_) => bridge.error("could not add that peer"),
        },

        HostToBridge::RemovePeer { mac } => match manager.remove_peer(&mac) {
            Ok(()) => bridge.log(LogLevel::Debug, "peer removed"),
            // Symmetric with `AddPeer` swallowing `PeerExists`: gone is the
            // outcome the host asked for. Freeing a slot is precisely the
            // idempotent path a full peer table sends the host down, and it
            // should not have to remember which peers it already gave up.
            Err(EspNowError::Error(esp_radio::esp_now::Error::NotFound)) => {
                bridge.log(LogLevel::Debug, "peer was already gone");
            }
            Err(_) => bridge.error("could not remove that peer"),
        },
    }
}

/// A plaintext station peer on whatever channel the radio is already using.
///
/// `channel: None` becomes 0, which ESP-NOW reads as "the current one". Setting
/// it explicitly would mean re-registering every peer whenever the host moves
/// the bridge with [`HostToBridge::SetChannel`].
const fn peer(mac: &Mac) -> PeerInfo {
    PeerInfo {
        interface: EspNowWifiInterface::Station,
        peer_address: *mac,
        // wartui does not do encrypted ESP-NOW at all, so there is no PMK to
        // derive and no LMK to carry. Nodes must have `use_encryption` off.
        lmk: None,
        channel: None,
        encrypt: false,
    }
}

/// Put one frame on the air and report what the radio made of it.
///
/// Blocks until the transmit callback fires, which is the whole point: the
/// vendor core clears its dirty flag from `esp_now_send`'s return value
/// (`src/WiFiOps.cpp:679`), so it believes every assignment it *enqueued* was
/// delivered. Unicast ESP-NOW is MAC-acknowledged, so waiting turns that guess
/// into a fact. `SendWaiter` busy-waits and its `Drop` waits too, so there is
/// no way to start a send and walk away — but the scheduler is preemptive, the
/// Wi-Fi task still runs, and the wait is milliseconds against a 300 ms window.
fn transmit(
    manager: &EspNowManager<'_>,
    sender: &mut EspNowSender<'_>,
    dst: &Mac,
    ensure_peer: bool,
    payload: &[u8],
) -> SendStatus {
    if !manager.peer_exists(dst) {
        if !ensure_peer {
            return SendStatus::NoPeer;
        }
        match manager.add_peer(peer(dst)) {
            Ok(()) => {}
            // The radio's table holds twenty entries in total, and one of them
            // is the broadcast peer `esp-radio` registers at init
            // (`esp_now/mod.rs:726`). That slot is worth more to a twentieth
            // node than it is to us: this bridge only ever *receives*
            // broadcasts, nodes send them, and ESP-NOW delivers a received
            // frame whether or not its sender is a peer. So give it up and try
            // once more. A second refusal means a fleet above the twenty
            // `MAX_NODES` wartui supports, which the host reports as such.
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
                if manager.remove_peer(&BROADCAST).is_err() {
                    return SendStatus::PeerTableFull;
                }
                match manager.add_peer(peer(dst)) {
                    Ok(()) | Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
                    Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
                        return SendStatus::PeerTableFull;
                    }
                    Err(_) => return SendStatus::Rejected,
                }
            }
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
            Err(_) => return SendStatus::Rejected,
        }
    }

    let waiter = match sender.send(dst, payload) {
        Ok(waiter) => waiter,
        Err(EspNowError::Error(esp_radio::esp_now::Error::NotFound)) => return SendStatus::NoPeer,
        Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
            return SendStatus::PeerTableFull;
        }
        Err(_) => return SendStatus::Rejected,
    };

    match waiter.wait() {
        // Broadcast is never acknowledged, so a success here means only that
        // the frame was sent. Saying so is more honest than reporting an ack
        // that no standard requires anyone to send.
        Ok(()) if *dst == BROADCAST => SendStatus::Broadcast,
        Ok(()) => SendStatus::AckOk,
        Err(_) => SendStatus::AckFail,
    }
}
