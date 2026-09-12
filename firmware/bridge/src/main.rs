//! wartui's USB ESP-NOW bridge.
//!
//! A laptop cannot speak ESP-NOW, so this dongle does it on the laptop's behalf:
//! it parks a radio on the mesh's control channel, forwards every frame it hears
//! up the USB link, and transmits frames the host hands it.
//!
//! It is deliberately dumb — COBS framing and `esp-radio`, and nothing about what
//! the bytes mean. Every rule that could turn out to be wrong lives on the host,
//! where it is unit-testable and a fix costs a `cargo run` rather than a reflash.
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
//! assignment from a hopeful one. Peers are added on demand and never removed.
//!
//! `README.md` has the flashing commands and the esp-hal version wall.
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
#[cfg(feature = "xiao-external-antenna")]
use esp_hal::gpio::{Level, Output, OutputConfig};
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
/// The channel the fleet speaks on. Shared with the node
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
#[cfg(all(feature = "xiao-external-antenna", not(feature = "esp32c6")))]
compile_error!("xiao-external-antenna drives a XIAO ESP32-C6's RF switch; build it with esp32c6");

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
/// A busy loop would work — the scheduler is preemptive — but would burn the core
/// for nothing. One millisecond is a hundred times finer than the 100 ms window
/// any of this has to hit.
const IDLE_SLEEP: Duration = Duration::from_millis(1);

// There is no watchdog here, having built one and measured that it cannot work.
// `esp_hal::init` disables every watchdog on the chip
// (`esp-hal-1.1.2/src/lib.rs:751-761`), and the RWDT that would close the gap
// provably never resets these parts on esp-hal 1.1.2 — it counts, unfed, and
// `WDT_PROCPU_RESET_EN` is the one enable that will not be written
// (`docs/phase-3-findings.md` has the register dumps).
//
// So a genuine hang in the loop below has nothing behind it and the board has to
// be unplugged: a real gap, left deliberately open, because a safety net that
// provably catches nothing is worse than an absent one — it will be trusted. The
// failure this project has actually seen was never a hang, and [`StallWatch`] is
// what guards it.

/// Where the loop was when it last stopped making progress.
///
/// In RTC fast memory and marked persistent, so it survives the resets that
/// matter — the panic handler's, the watchdog's, and the one [`StallWatch`]
/// asks for — and is read back by the next life. A power-on leaves it
/// undefined, which is why [`boot_phase`] only believes it when the reset
/// reason says the RTC domain was not reset.
///
/// An `AtomicU8` because a `static mut` cannot be written without `unsafe`, which is
/// denied here everywhere but [`set_peer_rate`]. Never contended: only the main task
/// touches it.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static PHASE: AtomicU8 = AtomicU8::new(0);

/// Says that [`PHASE`] was written by a build that writes it.
///
/// Persistent RTC memory holds whatever the last thing to use it left there — the
/// previous firmware's data after a reflash, nothing in particular after a
/// power-on — and either would be reported as a phase with a straight face. A word
/// this build alone writes is what separates a marker from a coincidence.
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
/// A power-on gives uninitialised RTC memory; anything else reached us through a
/// reset that left the RTC domain alone, so the marker is real.
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
/// `SocResetReason` names silicon blocks rather than causes, and the three chips
/// disagree about which blocks exist and what to call them. The C5 has
/// `PowerGlitch` and `CpuLockup` alone; the S3 *renames* the CPU-scoped variants
/// the RISC-V parts spell `Cpu0Sw`, `Cpu0Mwdt0`, `Cpu0Mwdt1` and `Cpu0RtcWdt`,
/// having two cores with neither privileged.
///
/// The numeric codes behind those names are identical on all three parts, so the
/// temptation to match on `reason as u8` and be done should be resisted: the enum
/// is the only thing that makes the next chip's differences visible. Matching only
/// the variants every chip defines was the first version here, and it silently
/// cost the C5 both of its own.
///
/// What is left unmapped is deliberate. `CoreDeepSleep` cannot happen: nothing
/// here sleeps. `CoreSDIO` and `CoreEfuseCrc` say nothing an operator could act on
/// beyond "this board is unwell", which [`ResetCause::Unknown`] already says.
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
        // A glitch on the supply rail is a brownout to anyone holding the board,
        // and the remedy printed for it is the right one.
        #[cfg(feature = "esp32c5")]
        SocResetReason::PowerGlitch => ResetCause::Brownout,
        #[cfg(feature = "esp32s3")]
        SocResetReason::CorePwrGlitch => ResetCause::Brownout,
        // The S3's *other* glitch detector: esp-hal names 0x17 "glitch on power"
        // and 0x13 "glitch on clock", and only the first wants a new cable.
        #[cfg(feature = "esp32s3")]
        SocResetReason::SysClkGlitch => ResetCause::ClockGlitch,
        // The only signal any of these parts gives for the hang class, and only the
        // C5 gives it — with no working watchdog behind it, a C6 or an S3 simply
        // does not report that class at all.
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

/// Reset the chip, undoing first what the C5's ROM leaves behind.
///
/// Every reset this firmware takes comes through here — the panic handler, the
/// stall detector and [`HostToBridge::Reset`] — because on the C5 a bare
/// `software_reset()` does not reboot the board, it ends it. The part comes up
/// with `PCR.RESET_EVENT_BYPASS.reset_event_bypass` set, which keeps a core reset
/// from also resetting the system bus; the ROM's own MSPI core reset then leaves
/// the AXI bus frozen. The banner prints, `SPI mode:` never does, and nothing
/// recovers the board until it loses power — measured four ways. That is strictly
/// worse than the wedge the stall detector exists to clear.
///
/// Clearing the bit is what ESP-IDF does on every boot and what `esp-hal` does in
/// its C5 `pre_init` from 1.2 onwards (esp-rs/esp-hal#5703), which the version
/// wall in `README.md` keeps out of reach. Written here rather than in [`main`]
/// because a panic can land before `main` reaches a line of its own, and one
/// funnel is one place to delete when that pin moves — see issue #16.
fn reboot() -> ! {
    #[cfg(feature = "esp32c5")]
    esp_hal::peripherals::PCR::regs()
        .reset_event_bypass()
        .modify(|_, w| w.reset_event_bypass().clear_bit());

    esp_hal::system::software_reset()
}

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
    /// The rule has been wrong twice, so it lives in [`wartui_proto::stall`].
    stall: StallWatch,
}

impl Bridge {
    /// Microseconds since boot, wrapping after roughly 71 minutes.
    ///
    /// The host only subtracts one of these from another, so the wrap is harmless
    /// and the narrower type keeps the frame small.
    fn now_us(&self) -> u32 {
        self.boot.elapsed().as_micros() as u32
    }

    /// Milliseconds since boot, which is the clock [`StallWatch`] runs on.
    ///
    /// Full width, unlike [`Bridge::now_us`]: this one is compared against
    /// timeouts, where a wrap would read as a host that spoke in the future.
    fn now_ms(&self) -> u64 {
        self.boot.elapsed().as_millis()
    }

    /// Say who we are.
    ///
    /// Sent at boot and again whenever the host asks, since the host is usually not
    /// attached at boot and this is how it learns the chip, MAC and link revision.
    fn announce(&mut self, mac: Mac) {
        self.outbox.send(&BridgeToHost::Ready {
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

    // Read before anything else writes it: this is the *previous* life's marker,
    // and `esp_hal::init` has already latched the reset reason to read it against.
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

    // A XIAO ESP32-C6's RF switch, powered, given 100 ms, then set to the U.FL
    // connector before the radio starts and held for the life of `main`.
    // `firmware/node/src/main.rs` has why.
    #[cfg(feature = "xiao-external-antenna")]
    let _antenna = {
        let power = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
        CurrentThreadHandle::get().delay(Duration::from_millis(100));
        (power, Output::new(peripherals.GPIO14, Level::High, OutputConfig::default()))
    };

    // `esp-radio`'s default is China under `WIFI_COUNTRY_POLICY_MANUAL`, which
    // refuses 5 GHz 100-144 outright. The bridge sits on channel 6 and would never
    // notice, but `--channel` is a plain `u8` an operator can point anywhere, and
    // a fleet moved somewhere this domain forbids would stop here while the nodes
    // went. This end at least says so — `SetChannel` answers a refusal with an
    // `Error` frame — but two halves of one fleet disagreeing about what is legal
    // is a trap even when one half can describe it.
    let mut controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");
    // Every radio in the fleet transmits at 2 dBm; `plan::TX_POWER_QUARTER_DBM` has why.
    // Before the split, which borrows the controller for the rest of the program, and
    // reported after `Ready` below, because the ROM banner swallows anything earlier.
    let tx_power = controller.set_max_tx_power(wartui_proto::plan::TX_POWER_QUARTER_DBM);
    // Split rather than kept whole: `EspNowSender::send` needs `&mut`, so holding
    // the parts separately keeps a transmit from borrowing the receive path.
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
    finish_radio_setup(&manager, &mut bridge, tx_power.is_ok());

    loop {
        let mut worked = false;
        mark(LoopPhase::DrainRadio);
        worked |= drain_radio(&receiver, &mut bridge);
        mark(LoopPhase::DrainLink);
        worked |= drain_link(&mut usb_rx, &manager, &mut sender, &mut bridge, mac);

        mark(LoopPhase::Pump);
        let moved = bridge.outbox.pump(&mut sink);
        worked |= moved;

        // Nothing subtler is available: the endpoint cannot be re-armed from this
        // end, and every way of telling the host goes out through the broken path.
        // So the reset *is* the message — the link drops, comes back, and the
        // `Ready` behind it says `TxStalled`.
        let queued = !bridge.outbox.is_empty();
        if bridge.stall.note_tx(moved, queued, bridge.now_ms()) {
            mark(LoopPhase::TxStalled);
            reboot();
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
            // It arrives unsigned: `wifi_pkt_rx_ctrl_t.rssi` is a signed 8-bit
            // bitfield, but the generated accessor extracts the bits unsigned and
            // transmutes, so -62 dBm reaches us as 194 and a clamp to `i8`
            // saturates every frame to 127. Reinterpreting the low byte recovers
            // it, and keeps working if the binding is ever fixed to sign-extend.
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
                // Proof of a host, and only a frame that decoded counts: a board
                // running node firmware talks constantly and none of it is a
                // frame, which `StallWatch` must not read as somebody waiting.
                bridge.stall.note_host(bridge.now_ms());
                handle(command, manager, sender, bridge, mac);
            }
            Err(err) => {
                // Expected after a reset, when the ROM banner arrives down the
                // same pipe. At debug, so a real version mismatch still shows.
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

        HostToBridge::Reset => reboot(),

        HostToBridge::SendEspNow { id, dst, ensure_peer, payload } => {
            // Named separately from `Command` because it is the one place the loop
            // can block unboundedly: `SendWaiter` busy-waits on a callback with no
            // timeout, so a `Transmit` phase says which call did not come back.
            mark(LoopPhase::Transmit);
            let status = transmit(manager, sender, &dst, ensure_peer, &payload);
            // Stamped *after* the transmit callback, not before the send. Against
            // the `rx_us` of the heartbeat that opened the window, this is the real
            // time from "the node is listening" to "the radio says it has it".
            let tx_us = bridge.now_us();
            bridge.outbox.send(&BridgeToHost::SendResult { id, status, tx_us });
        }

        HostToBridge::AddPeer { mac } => match add_peer(manager, &mac) {
            Ok(true) => bridge.log(LogLevel::Debug, "peer added"),
            Ok(false) => bridge.error("peer added, but its rate was refused; it stays at 1 Mbps"),
            // Already known is the outcome the host wanted, not a failure.
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
            Err(_) => bridge.error("could not add that peer"),
        },

        HostToBridge::RemovePeer { mac } => match manager.remove_peer(&mac) {
            Ok(()) => bridge.log(LogLevel::Debug, "peer removed"),
            // Symmetric with `AddPeer` swallowing `PeerExists`: gone is the outcome
            // the host asked for, and freeing a slot is the idempotent path a full
            // peer table sends it down.
            Err(EspNowError::Error(esp_radio::esp_now::Error::NotFound)) => {
                bridge.log(LogLevel::Debug, "peer was already gone");
            }
            Err(_) => bridge.error("could not remove that peer"),
        },
    }
}

/// A plaintext station peer on whatever channel the radio is already using.
///
/// `channel: None` becomes 0, which ESP-NOW reads as "the current one" — setting it
/// explicitly would mean re-registering every peer on a [`HostToBridge::SetChannel`].
const fn peer(mac: &Mac) -> PeerInfo {
    PeerInfo {
        interface: EspNowWifiInterface::Station,
        peer_address: *mac,
        // wartui does no encrypted ESP-NOW at all, so there is no PMK and no LMK.
        // Nodes must have `use_encryption` off.
        lmk: None,
        channel: None,
        encrypt: false,
    }
}

/// The radio setup that has to wait for `Ready`, because the ROM banner swallows any
/// frame sent before it.
///
/// Out of `main` because `main` sits at `.clippy.toml`'s stack threshold on the C5,
/// and every `Error` frame built there is another 276 bytes of its frame.
#[inline(never)]
fn finish_radio_setup(manager: &EspNowManager<'_>, bridge: &mut Bridge, tx_power_capped: bool) {
    if !tx_power_capped {
        bridge.error("could not cap transmit power at 2 dBm");
    }
    // The one peer not added through `add_peer`: `esp-radio` registers it at init.
    if !set_peer_rate(manager, &BROADCAST) {
        bridge.error("could not set the broadcast peer's rate; it stays at 1 Mbps");
    }
}

/// Register a peer and set the rate it is sent at, saying whether the rate took.
///
/// The funnel for every peer this bridge adds. The rate belongs to the peer entry,
/// so a peer registered any other way — or removed and registered again — is sent to
/// at 1 Mbps with nothing to say so. A refused rate is not an error here: that peer
/// is slower, not unreachable.
fn add_peer(manager: &EspNowManager<'_>, mac: &Mac) -> Result<bool, EspNowError> {
    manager.add_peer(peer(mac))?;
    Ok(set_peer_rate(manager, mac))
}

/// Send to `mac` at 802.11g 24 Mbps rather than ESP-NOW's 802.11b 1 Mbps default.
///
/// For airtime on the control channel. A 90-byte frame is about 910 µs at 1 Mbps
/// with its long preamble and about 50 µs at 24 Mbps, and every frame the fleet sends
/// is about that short, so the rate and preamble are the whole cost: a node's 6 ms
/// stagger slot in a twenty-node fleet is mostly empty air rather than mostly one
/// heartbeat. Not 802.11ax, which the S3 cannot decode and whose preamble costs more
/// than it saves on frames this short. Receivers need nothing; any 802.11b/g rate
/// decodes unannounced.
///
/// The price is sensitivity, roughly 10 dB against 1 Mbps, which a fleet sharing a
/// car has even at 2 dBm (`plan::TX_POWER_QUARTER_DBM`): on the bench, a C5 and a C6
/// beside this bridge arrived at −43 and −58 dBm and lost 0% and 2.3% of their
/// heartbeats over ten minutes.
///
/// Straight into IDF, and so the one `unsafe` in this firmware. `esp-radio`
/// 1.0.0-beta.0 wraps only the interface-wide `esp_wifi_config_espnow_rate`, which the
/// C5 and C6 Wi-Fi libraries refuse in Wi-Fi 6 mode (esp-hal #1612) — and whose
/// `WifiPhyRate` is numbered without IDF's gap at 4, so its `Rate24m` sends 48 Mbps.
/// `esp_now_set_peer_rate_config` is the per-peer call that replaced it, and taking
/// IDF's own constants sidesteps the numbering. `firmware/node/src/radio.rs` has the
/// node's copy.
///
/// The manager goes unused: it is proof that `esp_wifi_start` and `esp_now_init` have
/// both run, which IDF requires first.
#[allow(unsafe_code, reason = "the one IDF call esp-radio does not wrap")]
fn set_peer_rate(_manager: &EspNowManager<'_>, mac: &Mac) -> bool {
    #[cfg(feature = "esp32c5")]
    use esp_wifi_sys_esp32c5::include as sys;
    #[cfg(feature = "esp32c6")]
    use esp_wifi_sys_esp32c6::include as sys;
    #[cfg(feature = "esp32s3")]
    use esp_wifi_sys_esp32s3::include as sys;

    let mut config = sys::esp_now_rate_config_t {
        phymode: sys::wifi_phy_mode_t_WIFI_PHY_MODE_11G,
        rate: sys::wifi_phy_rate_t_WIFI_PHY_RATE_24M,
        ersu: false,
        dcm: false,
    };
    // SAFETY: `mac` is six readable bytes and `config` is a fully initialised
    // `esp_now_rate_config_t`, both alive for the whole call. ESP-NOW is initialised,
    // since an `EspNowManager` exists. 0 is `ESP_OK`.
    unsafe { sys::esp_now_set_peer_rate_config(mac.as_ptr(), &mut config) == 0 }
}

/// Put one frame on the air and report what the radio made of it.
///
/// Blocks until the transmit callback fires, which is the whole point.
/// `esp_now_send`'s return value says only that a frame was enqueued; unicast
/// ESP-NOW is MAC-acknowledged, so waiting turns a guess about delivery into a
/// fact. `SendWaiter` busy-waits and its `Drop` waits too, but the
/// scheduler is preemptive and the wait is milliseconds against a 100 ms window.
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
        match add_peer(manager, dst) {
            // A refused rate leaves the peer at 1 Mbps, which still delivers, and a
            // `SendStatus` has no way to say it.
            Ok(_) => {}
            // The radio's table holds twenty entries, one of them the broadcast
            // peer `esp-radio` registers at init (`esp_now/mod.rs:726`). That slot
            // is worth more to a twentieth node: this bridge only ever *receives*
            // broadcasts, and ESP-NOW delivers a received frame whether or not its
            // sender is a peer. So give it up and retry once. A second refusal
            // means a fleet above `MAX_NODES`, which the host reports as such.
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
                if manager.remove_peer(&BROADCAST).is_err() {
                    return SendStatus::PeerTableFull;
                }
                match add_peer(manager, dst) {
                    Ok(_) | Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
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
        // Broadcast is never acknowledged, so success here means only that the
        // frame was sent.
        Ok(()) if *dst == BROADCAST => SendStatus::Broadcast,
        Ok(()) => SendStatus::AckOk,
        Err(_) => SendStatus::AckFail,
    }
}
