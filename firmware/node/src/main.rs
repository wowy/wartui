//! wartui's wardriving node firmware.
//!
//! A node listens on one channel at a time, reports every access point it has not
//! reported lately, and takes its share of the channel pool from whatever is
//! acting as the mesh's core — for this fleet, a laptop running `wartui` behind a
//! USB bridge.
//!
//! A rewrite rather than a port: the web interface, SD card, display, buttons,
//! fuel gauge, GPS, geofencing, uploads and dock mode did not come across, because
//! every kilobyte of them is a kilobyte that can go wrong where a reflash is the
//! only way to find out.
//!
//! `README.md` § "What it does differently" is the longer account of its
//! design. In short: it *listens* rather than scanning, so it
//! never transmits on a DFS channel and can hold `sniffer()` and `esp_now()` at
//! once; it returns to the control channel after every dwell rather than once a
//! sweep; an unassigned node parks rather than sweeping the whole table; the node
//! given the Bluetooth scan sniffs nothing and holds the control channel for its
//! whole second; and every heartbeat says what this build can do. Its wire format
//! is wartui's own in both directions — [`wartui_proto::air`].
//!
//! The runner is `espflash flash --monitor`, and unlike the bridge the monitor is
//! worth watching: a node's USB endpoint carries nothing but diagnostics.
#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe with esp-hal types, especially those holding \
    buffers for the duration of a transfer"
)]
#![deny(clippy::large_stack_frames)]

use esp_hal::clock::CpuClock;
#[cfg(feature = "xiao-external-antenna")]
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::esp_now::{EspNowManager, EspNowReceiver, EspNowSender};
use esp_radio::wifi::{ControllerConfig, WifiController};
use esp_rtos::CurrentThreadHandle;
use static_cell::ConstStaticCell;
#[cfg(feature = "ble")]
use static_cell::StaticCell;
use wartui_proto::air::{
    AdminMsg, Capabilities, DecodeError, Frame, HeartbeatMsg, SIGHTING_MSG_MAX,
};
use wartui_proto::dedup::MacRing;
use wartui_proto::plan::{
    ADMIN_WAIT_MS, BLE_BEAT_MS, CHANNEL_DWELL_MS, CONTROL_CHANNEL, ChannelSet, DEDUP_RING,
    IDLE_BEAT_MS, NODE_STAGGER_WINDOW_MS, NUM_SCAN_CHANNELS, SCAN_CHANNELS, SweepCursor,
    stagger_offset_ms,
};

#[cfg(feature = "ble")]
mod ble;
mod radio;
mod sniff;

// This creates the app descriptor the esp-idf bootloader expects.
esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;

#[cfg(not(any(feature = "esp32c5", feature = "esp32c6")))]
compile_error!("select a chip: --features esp32c5 or --features esp32c6");
#[cfg(all(feature = "esp32c5", feature = "esp32c6"))]
compile_error!("select exactly one chip: esp32c5 and esp32c6 are mutually exclusive");
#[cfg(all(feature = "xiao-external-antenna", not(feature = "esp32c6")))]
compile_error!("xiao-external-antenna drives a XIAO ESP32-C6's RF switch; build it with esp32c6");

/// Say something on the USB endpoint, if this build has anywhere to say it.
///
/// The bridge is forbidden from printing — its link protocol runs over the same
/// endpoint — but a node has that pipe to itself, so this is the one window into a
/// device with no display, no buttons and no web interface.
///
/// `esp-println`'s serial-JTAG writer gives the FIFO a bounded number of attempts
/// and then remembers that nobody is draining it: losing a line is correct, losing
/// the sweep is not. The arguments are type-checked either way, so a `log`-less
/// build cannot rot a format string nobody compiled.
macro_rules! note {
    ($($arg:tt)*) => {{
        #[cfg(feature = "log")]
        esp_println::println!($($arg)*);
        #[cfg(not(feature = "log"))]
        {
            let _ = format_args!($($arg)*);
        }
    }};
}

/// Granularity of the listening loops. Fine enough that a 100 ms window is not
/// meaningfully shortened, coarse enough not to spin the core.
const POLL_MS: u64 = 2;

/// Reset the chip, undoing first what the C5's ROM leaves behind.
///
/// The same funnel the bridge has, with the same one register in it;
/// `firmware/bridge/src/main.rs` carries the reasoning. The exposure is quieter on
/// this end — a node that never comes back reads as `no heartbeat`, which is also
/// what a node out of range reads as — and quieter is why it would go unexplained
/// for longer.
fn reboot() -> ! {
    #[cfg(feature = "esp32c5")]
    esp_hal::peripherals::PCR::regs()
        .reset_event_bypass()
        .modify(|_, w| w.reset_event_bypass().clear_bit());

    esp_hal::system::software_reset()
}

/// Resets rather than hanging, for the same reason the bridge does: a node that
/// has stopped is indistinguishable from one out of range, and a reset at least
/// restarts the heartbeat counter, which the host reads as `rebooted` and
/// answers with a fresh assignment.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    note!("panic: {}", info);
    reboot()
}

// No watchdog here either; `firmware/bridge/src/main.rs` gives the reason. The gap
// costs more on this end, because a hung node reads as `no heartbeat` and so does
// one out of range or with a flat battery.

/// Everything that changes while the node runs.
///
/// Built at compile time into a [`ConstStaticCell`] rather than on the stack:
/// the dedup ring alone is two and a half kilobytes, and `StaticCell::init_with`
/// would still construct it in a frame before moving it.
struct Node {
    /// The epoch of the assignment held. Zero means none has ever arrived, which
    /// the core never puts on the wire.
    version: u8,
    /// Slot in the fleet-wide staggering order.
    node_index: u8,
    /// Fleet size the stagger is computed against.
    node_count: u8,
    /// Which [`SCAN_CHANNELS`] indices to dwell on, in ascending order.
    channels: ChannelSet,
    /// Whether the core asked this node to scan Bluetooth as well.
    ble: bool,
    /// Where the sweep has got to in those indices.
    cursor: SweepCursor,
    /// Monotonic from boot. Its going backwards is how the host notices a node
    /// has restarted and forgotten its assignment.
    counter: u32,
    seen: MacRing<DEDUP_RING>,
    reported: u32,
}

impl Node {
    // Only ever evaluated at compile time, into `NODE`, so the frame this lint
    // measures is never on a stack.
    #[allow(clippy::large_stack_frames)]
    const fn new() -> Self {
        Self {
            version: 0,
            node_index: 0,
            node_count: 1,
            channels: ChannelSet::empty(),
            ble: false,
            cursor: SweepCursor::new(),
            counter: 1,
            seen: MacRing::new(),
            reported: 0,
        }
    }

    /// Whether the node has anything to do, given whether it has a scan to run.
    ///
    /// Channels, or the Bluetooth scan. An empty mask with the flag clear counts as
    /// nothing rather than as an error: no frame means "scan nothing", so this needs a
    /// confused host, and parking is the same answer as never having been told anything.
    const fn assigned(&self, scanning: bool) -> bool {
        self.version != 0 && (!self.channels.is_empty() || self.bluetooth_only(scanning))
    }

    /// Whether this node's whole job is Bluetooth.
    ///
    /// `scanning` is whether there is a scan to run at all: the `ble` feature, and a
    /// controller that started. A flag that reaches a node with neither leaves it parked
    /// and reporting nothing — honest, and diagnosable from the adoption line — rather
    /// than holding the control channel for a scan that cannot happen. The core announces
    /// what this node can do and so never asks; this is the node not depending on that,
    /// and the reason the answer is passed in rather than read off a `cfg!`, which would
    /// cover the build without the feature and miss the radio that would not start.
    const fn bluetooth_only(&self, scanning: bool) -> bool {
        scanning && self.ble && self.channels.is_empty()
    }

    /// Take an assignment, if it is not the one already held.
    ///
    /// The test is `!=` rather than `>`, so a host that restarted and went back to
    /// epoch 1 is still believed — and so re-sending an identical assignment is
    /// acknowledged and silently discarded.
    ///
    /// Nothing is rejected: `ChannelSet` drops bits above [`NUM_SCAN_CHANNELS`]
    /// rather than refusing the frame, because the radio has already
    /// MAC-acknowledged it and a node that quietly declined would leave the two
    /// sides disagreeing with nothing to say so.
    fn adopt(&mut self, admin: &AdminMsg) -> bool {
        if admin.epoch == self.version {
            return false;
        }
        self.version = admin.epoch;
        self.node_index = admin.node_index;
        self.node_count = admin.node_count;
        self.channels = admin.channels;
        self.ble = admin.scan_ble();
        self.cursor = SweepCursor::new();
        true
    }

    /// Step to the next assigned channel, saying whether that completed a sweep.
    fn advance(&mut self) -> bool {
        self.cursor.advance(self.channels)
    }

    /// The channel the cursor is on, or `None` when an assignment has been
    /// adopted and its sweep has not started.
    fn channel(&self) -> Option<u8> {
        self.cursor.index().map(|idx| SCAN_CHANNELS[usize::from(idx.min(NUM_SCAN_CHANNELS - 1))])
    }
}

/// What this node is, as every heartbeat says it.
///
/// `ble` is whether there is a controller here to scan with, not whether it is
/// scanning: the scan is the core's decision and is off at every boot. Read off the
/// controller rather than off the cargo feature, because the core deals *no channels*
/// to the node it asks for Bluetooth — so a node claiming a scan it cannot run would
/// sniff nothing either, and a `ble` build whose controller refused to start is
/// exactly that node. `5g` is the chip, and is what keeps the planner from dealing a
/// C6 a share nobody scans.
fn capabilities(bluetooth: bool) -> Capabilities {
    Capabilities::here(bluetooth, cfg!(feature = "esp32c5"))
}

static NODE: ConstStaticCell<Node> = ConstStaticCell::new(Node::new());

#[cfg(feature = "ble")]
static SCANNER: StaticCell<ble::Scanner<'static>> = StaticCell::new();

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    note!("reset reason: {:?}", esp_hal::system::reset_reason());

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    // A XIAO ESP32-C6 routes the chip's one RF pin through a switch between an
    // onboard ceramic antenna and a U.FL connector, and nothing in the radio stack
    // knows it is there. Left undriven the switch stays on the ceramic antenna, so
    // an antenna on the connector is decoration and the node reads as one with a
    // weak radio rather than a wrong setting. GPIO3 low powers the switch; GPIO14
    // high selects the connector, 100 ms later, as Seeed's own firmware does — the
    // bench worked without the wait, but a switch still powering up when it is told
    // which way to point is not a failure this node could report. Set before the
    // radio starts, so its first frame leaves by the antenna the build asked for,
    // and bound for the life of `main`, which never returns, so the pins are never
    // handed back.
    #[cfg(feature = "xiao-external-antenna")]
    let _antenna = {
        let power = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
        CurrentThreadHandle::get().delay(Duration::from_millis(100));
        (power, Output::new(peripherals.GPIO14, Level::High, OutputConfig::default()))
    };

    // Order matters. Configuring the controller needs `&mut`, while `sniffer()` and
    // `esp_now()` each borrow it for as long as they live — so everything mutable
    // happens first and both handles are then held for the rest of the program.
    // That is the whole reason this node sniffs rather than scans: `scan_async`
    // also wants `&mut`, and holding it alongside ESP-NOW is not expressible.
    //
    // `US` rather than `esp-radio`'s default, which silently costs a C5 channels
    // 100-144 — `firmware/bridge/src/main.rs` has the detail.
    let mut controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");

    // The C6 is 2.4 GHz only and has no `BandMode::Auto` to select. On the C5,
    // without this, every 5 GHz channel in the pool is refused.
    #[cfg(feature = "esp32c5")]
    match controller.set_band_mode(esp_radio::wifi::BandMode::Auto) {
        Ok(()) => {}
        Err(err) => note!("could not enable dual band: {:?}", err),
    }

    // The standalone fallback is 2 dBm. A host replaces it through the first
    // assignment it sends; `DEFAULT_TX_POWER_QUARTER_DBM` documents the default.
    match controller.set_max_tx_power(wartui_proto::plan::DEFAULT_TX_POWER_QUARTER_DBM) {
        Ok(()) => {}
        Err(err) => note!("could not cap transmit power: {:?}", err),
    }

    let mut sniffer = controller.sniffer();
    sniffer.set_receive_cb(sniff::on_frame);
    let (manager, mut sender, receiver) = controller.esp_now().split();
    if !radio::set_broadcast_rate(&manager) {
        note!("could not set the ESP-NOW rate; broadcasting at 1 Mbps");
    }

    // Brought up before the loop rather than on demand: initialising a radio
    // between a dwell and an admin window is the kind of surprise to avoid.
    #[cfg(feature = "ble")]
    let mut scanner = match ble::Scanner::new(peripherals.BT) {
        Some(scanner) => Some(SCANNER.init(scanner)),
        None => {
            // Announced as no `ble` below, so the core never asks this node for the
            // scan — which matters more than it used to, since asking would leave it
            // sniffing nothing either.
            note!("bluetooth controller would not start; sniffing Wi-Fi only");
            None
        }
    };
    #[cfg(not(feature = "ble"))]
    let scanner: Option<()> = None;
    let scanning = scanner.is_some();
    let capabilities = capabilities(scanning);

    let node = NODE.take();
    let mac = esp_radio::wifi::Interface::station().mac_address();
    note!(
        "wartui node {} v{}, control channel {}",
        MacFmt(mac),
        env!("CARGO_PKG_VERSION"),
        CONTROL_CHANNEL
    );

    loop {
        if !node.assigned(scanning) {
            // Parked: reachable the whole time, collecting nothing. Heartbeat
            // first and listen afterwards, because the host only ever transmits
            // an assignment in answer to one — so this is how long joining a
            // fleet takes, and there is nothing to be gained by waiting first.
            //
            // Only heartbeat if the radio got there, for the reason the sweep
            // gives below. The listen runs either way: it is what keeps this
            // loop from spinning.
            if radio::park(&manager, &sniffer, CONTROL_CHANNEL, false) {
                heartbeat(&mut sender, node, capabilities);
            } else {
                note!("radio would not park on channel {}", CONTROL_CHANNEL);
            }
            listen(&manager, &receiver, node, IDLE_BEAT_MS);
            continue;
        }

        if node.bluetooth_only(scanning) {
            // Bluetooth is this node's whole job, so it sniffs nothing and never
            // leaves the control channel: it is the one node an assignment can
            // always reach, and the only thing the Bluetooth controller takes the
            // shared 2.4 GHz antenna from is this node's own heartbeat.
            //
            // Scan start to scan start is `BLE_BEAT_MS`. The scan first, because
            // the reports are what the heartbeat should be followed by; the window
            // last, because that is the moment the core answers in.
            let cycle = Instant::now() + Duration::from_millis(u64::from(BLE_BEAT_MS));
            if radio::park(&manager, &sniffer, CONTROL_CHANNEL, false) {
                // Always `Some` here: `scanning` is what put this node in this arm,
                // and it is that scanner being there.
                #[cfg(feature = "ble")]
                if let Some(scanner) = scanner.as_deref_mut() {
                    report_ble(&mut sender, node, scanner);
                }
                let stagger =
                    stagger_offset_ms(node.node_index, node.node_count, NODE_STAGGER_WINDOW_MS);
                if stagger > 0 {
                    CurrentThreadHandle::get().delay(Duration::from_millis(u64::from(stagger)));
                }
                heartbeat(&mut sender, node, capabilities);
            } else {
                note!("radio would not park on channel {}", CONTROL_CHANNEL);
            }
            // Whatever is left of the second, held open. A busy room that overran
            // leaves only the floor here and the next scan starts at once, which is
            // the right trade: a window missed costs one cycle, and the core's
            // re-send waits on the next heartbeat either way.
            listen(&manager, &receiver, node, remaining_ms(cycle).max(ADMIN_WAIT_MS));
            continue;
        }

        // Arm after the hop, disarm before the next one. `radio::park` has
        // promiscuous mode on across the channel change, so anything heard inside
        // it belongs to neither channel — and a refusal means the radio never
        // arrived, so collecting then would file this channel's name on whatever
        // it is still tuned to.
        let Some(channel) = node.channel() else {
            // An assignment adopted while parked, whose sweep has not begun: step
            // onto its lowest channel rather than whichever index the cursor held
            // when the frame arrived, which may no longer be assigned.
            node.advance();
            continue;
        };
        if radio::park(&manager, &sniffer, channel, true) {
            sniff::arm(channel);
            CurrentThreadHandle::get().delay(Duration::from_millis(u64::from(CHANNEL_DWELL_MS)));
            sniff::disarm();
        } else {
            note!("radio refused channel {}", channel);
        }

        // Back where the fleet can be heard, for as long as it takes to say what
        // was on that channel. A refusal here costs the observations rather than
        // misdirecting them.
        let on_control = radio::park(&manager, &sniffer, CONTROL_CHANNEL, false);
        if on_control {
            report(&mut sender, node, channel);
            drain_admin(&manager, &receiver, node);
        } else {
            note!("radio would not return to channel {}", CONTROL_CHANNEL);
        }

        // `advance` runs either way, so a radio that refused one hop does not leave
        // the node dwelling there for ever. Everything else needs the control
        // channel: a heartbeat sent from a dwell channel is not heard and, worse,
        // is *counted* — the local transmit succeeds and the counter climbs, so the
        // host sees an unbroken sequence instead of the reboot-shaped gap that
        // would make it re-issue. Silence is the honest report.
        if node.advance() && on_control {
            let stagger =
                stagger_offset_ms(node.node_index, node.node_count, NODE_STAGGER_WINDOW_MS);
            if stagger > 0 {
                CurrentThreadHandle::get().delay(Duration::from_millis(u64::from(stagger)));
            }
            heartbeat(&mut sender, node, capabilities);
            listen(&manager, &receiver, node, ADMIN_WAIT_MS);
        }
    }
}

/// How much of a deadline is left, in milliseconds, saturating at zero.
///
/// The Bluetooth cycle is a deadline rather than a duration: what is left of its
/// second after the scan, the reports and the heartbeat is whatever is left.
#[allow(clippy::cast_possible_truncation)]
fn remaining_ms(deadline: Instant) -> u32 {
    let now = Instant::now();
    if now >= deadline { 0 } else { (deadline - now).as_millis() as u32 }
}

/// Milliseconds since boot, as the dedup ring counts them. Truncation is the ring's
/// own wrap.
#[allow(clippy::cast_possible_truncation)]
fn now_ms() -> u32 {
    Instant::now().duration_since_epoch().as_millis() as u32
}

/// Broadcast one heartbeat, which is also the whole of this node's liveness.
///
/// Once per completed sweep, not on a timer. The
/// host reads the period as a rough measure of how many channels the node is
/// carrying, and treats sixty seconds of silence as a node that has left the
/// fleet.
fn heartbeat(sender: &mut EspNowSender<'_>, node: &mut Node, capabilities: Capabilities) {
    // Every heartbeat carries the capabilities, not just the first: sent once they
    // would be lost to a dropped frame or stale after a reflash, and they are three
    // bytes of thirteen.
    let msg = HeartbeatMsg { counter: node.counter, capabilities };
    if radio::broadcast(sender, &msg.encode()) {
        node.counter = node.counter.wrapping_add(1).max(1);
    }
}

/// Report everything heard on `channel` that has not been reported lately.
fn report(sender: &mut EspNowSender<'_>, node: &mut Node, channel: u8) {
    let mut sent = 0u32;
    let now = now_ms();
    while let Some(sighting) = sniff::take() {
        let rssi = Some(sighting.rssi);
        if !node.seen.is_due(&sighting.bssid, rssi, now) {
            continue;
        }
        let mut frame = [0u8; SIGHTING_MSG_MAX];
        let Some(len) = sighting.as_msg().encode_into(&mut frame) else { continue };
        // Recorded only once it is on the air: suppressing an access point the host
        // never received would hide it until the refresh.
        if radio::broadcast(sender, &frame[..len]) {
            node.seen.record(sighting.bssid, rssi, now);
            sent += 1;
        }
    }
    if sent > 0 {
        node.reported = node.reported.wrapping_add(sent);
        note!(
            "ch {}: {} new, {} total, {} dropped",
            channel,
            sent,
            node.reported,
            sniff::dropped()
        );
    }
}

/// Listen for advertisers and report the new ones.
///
/// Called once per [`BLE_BEAT_MS`] on the node whose whole job this is, at the top of
/// its cycle and before the heartbeat, so the Bluetooth controller is off again well
/// ahead of the window this node has to answer an assignment in. The node sniffs no
/// Wi-Fi, so the scan competes with nothing but that heartbeat for the antenna.
#[cfg(feature = "ble")]
fn report_ble(sender: &mut EspNowSender<'_>, node: &mut Node, scanner: &mut ble::Scanner<'_>) {
    let mut lines = 0u32;
    let heard = scanner.sweep();
    let now = now_ms();
    // One report at a time out of the ring, so the lock is never held across a
    // transmit — the same shape the Wi-Fi sightings are drained in.
    while let Some(report) = ble::take() {
        let rssi = report.has_rssi().then_some(report.rssi);
        if !node.seen.is_due(&report.address, rssi, now) {
            continue;
        }
        let mut frame = [0u8; SIGHTING_MSG_MAX];
        let Some(len) = report.encode_into(&mut frame) else { continue };
        // After the broadcast, for the reason `report` gives.
        if radio::broadcast(sender, &frame[..len]) {
            node.seen.record(report.address, rssi, now);
            lines += 1;
        }
    }
    if heard > 0 {
        node.reported = node.reported.wrapping_add(lines);
        note!("ble: {} heard, {} new, {} dropped", heard, lines, ble::dropped());
    }
}

/// Hold the control channel for `ms`, acting on anything that arrives.
///
/// This is the admin window. The radio acknowledges a unicast assignment in
/// hardware either way, but adopting one needs the frame, read only here.
fn listen(manager: &EspNowManager<'_>, receiver: &EspNowReceiver<'_>, node: &mut Node, ms: u32) {
    let until = Instant::now() + Duration::from_millis(u64::from(ms));
    loop {
        drain_admin(manager, receiver, node);
        if Instant::now() >= until {
            return;
        }
        CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
    }
}

/// Take whatever the radio has queued and adopt any assignment in it.
fn drain_admin(manager: &EspNowManager<'_>, receiver: &EspNowReceiver<'_>, node: &mut Node) {
    while let Some(received) = receiver.receive() {
        let admin = match Frame::decode(received.data()) {
            Ok(Frame::Admin(admin)) => admin,
            // A host whose assignment is not the shape this build reads — a
            // different wire version, or a different length under the same one.
            // Said out loud rather than dropped with everything else: it is the
            // whole diagnosis for a node that is talked to and never answers.
            Err(DecodeError::BadVersion(version)) => {
                note!("ignoring a frame at wire version {}; reflash this node", version);
                continue;
            }
            Err(DecodeError::BadLength { need, got }) => {
                note!(
                    "ignoring a {} byte assignment, this build reads {}; reflash this node",
                    got,
                    need
                );
                continue;
            }
            // Everything else on this channel: our own broadcasts coming back, the
            // rest of the fleet's, and whatever else is nearby — a vendor fleet's
            // frames fail the magic and land here, which is the point.
            _ => continue,
        };
        if node.adopt(&admin) {
            if !radio::set_tx_power(manager, admin.tx_power) {
                note!(
                    "could not set transmit power to {} quarter-dBm; retaining the previous power",
                    admin.tx_power
                );
            }
            note!(
                "assigned v{}: {} channels ({}), ble {}, node {} of {}, power {} quarter-dBm",
                node.version,
                node.channels.len(),
                Channels(node.channels),
                if node.ble { "on" } else { "off" },
                node.node_index,
                node.node_count,
                admin.tx_power
            );
        }
    }
}

/// A channel set, said in channel numbers rather than table indices.
///
/// The indices are what the wire carries, but the number written on every other
/// tool the operator owns is the channel — and the one line this prints is read
/// beside `wartui`'s own fleet table, which says channels too.
struct Channels(ChannelSet);

impl core::fmt::Display for Channels {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `none` rather than nothing: the one assignment with an empty set is the
        // Bluetooth node's, and a blank between the brackets reads as a bug.
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (n, idx) in self.0.indices().enumerate() {
            if n > 0 {
                f.write_str(",")?;
            }
            write!(f, "{}", SCAN_CHANNELS[usize::from(idx)])?;
        }
        Ok(())
    }
}

/// A MAC address, for the one line that prints one.
struct MacFmt([u8; 6]);

impl core::fmt::Display for MacFmt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, octet) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{octet:02X}")?;
        }
        Ok(())
    }
}
