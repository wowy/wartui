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
//! ```
//!
//! The runner is `espflash flash --monitor` with no `--chip`, so it detects the
//! part itself. Note that `--monitor` prints the framed link bytes as text and
//! will look like noise; use `wartui sniff` instead.

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
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_radio::esp_now::{
    EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface, PeerInfo,
};
use esp_rtos::CurrentThreadHandle;
use static_cell::StaticCell;
use wartui_proto::heapless::Vec;
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LogLevel,
    LogStr, MAX_FRAME, Mac, SendStatus, ShortStr, decode_frame,
};
use wartui_proto::outbox::{ByteSink, Outbox};

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6")))]
compile_error!("select a chip: --features esp32c5 or --features esp32c6");
#[cfg(all(feature = "esp32c5", feature = "esp32c6"))]
compile_error!("select exactly one chip: esp32c5 and esp32c6 are mutually exclusive");

#[cfg(feature = "esp32c5")]
const CHIP: Chip = Chip::Esp32C5;
#[cfg(feature = "esp32c6")]
const CHIP: Chip = Chip::Esp32C6;

/// Where the stock mesh lives (`src/WiFiOps.cpp:15`). The host can move us with
/// [`HostToBridge::SetChannel`], but nothing else will follow.
const DEFAULT_CHANNEL: u8 = 6;

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

    let controller = esp_radio::wifi::WifiController::new(peripherals.WIFI, Default::default())
        .expect("Wi-Fi controller");
    // Split rather than kept whole: `EspNowSender::send` needs `&mut`, and
    // holding the manager and receiver separately means a transmit does not
    // have to borrow the parts that answer `GetStatus` and drain the radio.
    let (manager, mut sender, receiver) = controller.esp_now().split();

    let mut bridge = Bridge {
        outbox: OUTBOX.init_with(Outbox::new),
        accumulator: FrameAccumulator::new(),
        channel: DEFAULT_CHANNEL,
        rx_count: 0,
        boot: Instant::now(),
    };

    match manager.set_channel(DEFAULT_CHANNEL) {
        Ok(()) => {}
        Err(_) => bridge.error("could not park the radio on the default channel"),
    }

    let mac = esp_radio::wifi::Interface::station().mac_address();
    bridge.announce(mac);

    loop {
        let mut worked = false;
        worked |= drain_radio(&receiver, &mut bridge);
        worked |= drain_link(&mut usb_rx, &manager, &mut sender, &mut bridge, mac);
        worked |= bridge.outbox.pump(&mut sink);

        if !worked {
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
            Ok(command) => handle(command, manager, sender, bridge, mac),
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
            // The radio's table holds twenty and the vendor firmware's node
            // table holds twenty-four, so a large fleet can reach this. Which
            // peer to give up is a policy question, and policy lives on the
            // host: it can free a slot with `RemovePeer` and try again.
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
                return SendStatus::PeerTableFull;
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
