//! wartui's wardriving node firmware.
//!
//! A node listens on one channel at a time, reports every access point it has
//! not reported lately, and takes its share of the channel pool from whatever
//! is acting as the mesh's core — which, for this fleet, is a laptop running
//! `wartui` behind a USB bridge.
//!
//! It is a rewrite rather than a port. The scanning and the ESP-NOW comms come
//! from the vendor firmware at `ESP32DualBandWardriver`, cited throughout by
//! `file:line`; the web interface, SD card, display, buttons, fuel gauge, GPS,
//! geofencing, uploads and dock mode do not, because a node in this fleet has
//! no use for any of them and every kilobyte of them is a kilobyte that can go
//! wrong somewhere a reflash is the only way to find out.
//!
//! Three things are deliberately different from the firmware it replaces, and
//! each of them is a fix rather than a preference.
//!
//! **It listens instead of scanning.** `WiFi.scanNetworks` is called with
//! `passive = false` throughout the vendor tree (`src/WiFiOps.cpp:745,755,779`),
//! so a stock node transmits a probe request on every channel it is assigned,
//! including the DFS channels where the rules say listen and do not speak. This
//! one parks the radio and reads the beacons, which is quieter, legal on 52-144,
//! and — because `WifiController::sniffer()` and `::esp_now()` both borrow the
//! controller immutably — able to coexist with ESP-NOW rather than tearing it
//! down and rebuilding it around every scan.
//!
//! **It is on the control channel far more often.** A stock node is deaf to its
//! core for all but the 300 ms it holds open once per sweep, because the scan
//! owns the radio for everything else. Here nothing owns the radio: the node
//! returns to the control channel after *every* dwell to report what it heard,
//! and an assignment sent at any of those moments lands. The 300 ms window is
//! still honoured, so the host's timing model is unchanged; it simply stops
//! being the only chance.
//!
//! **An unassigned node waits rather than sweeping.** The vendor default is all
//! forty channels (`src/WiFiOps.cpp:77-80`), which means a node that has never
//! heard a core duplicates whatever the rest of the fleet is doing and is
//! addressable for 300 ms per sweep while it does. This one parks on the
//! control channel and heartbeats until it is told what to scan. With wartui's
//! planner running that lasts a single heartbeat; under `--manual` it lasts
//! until a key is pressed, and the node collects nothing until then.
//!
//! ## Flashing
//!
//! ```bash
//! cargo run --release --features esp32c6   # or --features esp32c5
//! ```
//!
//! The runner is `espflash flash --monitor` with no `--chip`, so it detects the
//! part itself. Unlike the bridge, the monitor is worth watching: a node's USB
//! endpoint carries nothing but diagnostics.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe with esp-hal types, especially those holding \
    buffers for the duration of a transfer"
)]
#![deny(clippy::large_stack_frames)]

use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::esp_now::{EspNowReceiver, EspNowSender};
use esp_radio::wifi::{ControllerConfig, WifiController};
use esp_rtos::CurrentThreadHandle;
use static_cell::StaticCell;
use wartui_proto::air::{AdminMsg, Frame, MsgType, TextMsg, WARDRIVE_LINE_MAX};
use wartui_proto::dedup::MacRing;
use wartui_proto::plan::{
    ADMIN_WAIT_MS, CHANNEL_DWELL_MS, CONTROL_CHANNEL, DEDUP_RING, NODE_STAGGER_WINDOW_MS,
    NUM_SCAN_CHANNELS, SCAN_CHANNELS, stagger_offset_ms,
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

/// Say something on the USB endpoint, if this build has anywhere to say it.
///
/// The bridge is forbidden from printing, because its link protocol runs over
/// the same endpoint and a stray line would corrupt the framing. A node has that
/// pipe to itself — the vendor firmware's serial is output-only for the same
/// reason — so this is the one window into a device with no display, no buttons
/// and no web interface.
///
/// `esp-println`'s serial-JTAG writer gives the FIFO a bounded number of
/// attempts and then remembers that nobody is draining it, so a node with no
/// monitor attached drops its diagnostics rather than stalling mid-sweep. That
/// is the property that matters here: losing a line is correct, losing the
/// sweep is not.
///
/// The arguments are type-checked either way, so a `log`-less build cannot rot
/// a format string nobody compiled.
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

/// How long an unassigned node listens between heartbeats.
///
/// Comfortably longer than [`ADMIN_WAIT_MS`], because a parked node has nothing
/// else to do: the window is the whole of its cycle rather than a slice of it.
/// Short enough that joining a fleet is a second rather than a sweep.
const IDLE_BEAT_MS: u32 = 1000;

const _: () = assert!(
    IDLE_BEAT_MS >= ADMIN_WAIT_MS,
    "a parked node must hold the control channel for at least a full admin window"
);

/// Shortest gap between Bluetooth sweeps.
///
/// A sweep is nominally once per completed pass over the assigned channels, but
/// an assignment can be a single channel — with a full fleet on the 34-channel
/// US pool that is the ordinary outcome of the planner, and narrowing a node by
/// hand is a documented workflow. Such a node completes a pass every 125 ms, so
/// tying the Bluetooth scan to the pass would have it hold the shared 2.4 GHz
/// antenna for four fifths of its life, immediately before every admin window
/// it has to answer in — which is exactly the failure `docs/phase-0-findings.md`
/// measured on a stock node and the reason this firmware exists.
///
/// So the sweep is rate-limited to what a node carrying the whole pool would
/// reach anyway. A node on one channel then scans Bluetooth no more often than
/// a node on forty, and no assignment size makes it the dominant cost.
#[cfg(feature = "ble")]
const BLE_INTERVAL_MS: u64 = NUM_SCAN_CHANNELS as u64 * CHANNEL_DWELL_MS as u64;

/// Granularity of the listening loops. Fine enough that a 300 ms window is not
/// meaningfully shortened, coarse enough not to spin the core.
const POLL_MS: u64 = 2;

/// Resets rather than hanging, for the same reason the bridge does: a node that
/// has stopped is indistinguishable from one out of range, and a reset at least
/// restarts the heartbeat counter, which the host reads as `rebooted` and
/// answers with a fresh assignment.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    note!("panic: {}", info);
    esp_hal::system::software_reset()
}

/// Everything that changes while the node runs.
///
/// Boxed into `.bss` through a [`StaticCell`] rather than built on the stack:
/// the dedup ring alone is twelve hundred bytes.
struct Node {
    /// The epoch of the assignment held. Zero means none has ever arrived,
    /// which the vendor firmware also uses as its starting value
    /// (`src/WiFiOps.cpp:81`) and which the core never puts on the wire.
    version: u8,
    /// Slot in the fleet-wide staggering order.
    node_index: u8,
    /// Fleet size the stagger is computed against.
    node_count: u8,
    /// First and last [`SCAN_CHANNELS`] index to sweep, inclusive.
    start: u8,
    end: u8,
    /// Where in that range the sweep has got to.
    cursor: u8,
    /// Monotonic from boot. Its going backwards is how the host notices a node
    /// has restarted and forgotten its assignment.
    counter: u32,
    seen: MacRing<DEDUP_RING>,
    reported: u32,
}

impl Node {
    const fn new() -> Self {
        Self {
            version: 0,
            node_index: 0,
            node_count: 1,
            start: 0,
            end: NUM_SCAN_CHANNELS - 1,
            cursor: 0,
            counter: 1,
            seen: MacRing::new(),
            reported: 0,
        }
    }

    /// Whether the node has been told what to scan.
    const fn assigned(&self) -> bool {
        self.version != 0
    }

    /// Take an assignment, if it is not the one already held.
    ///
    /// The test is `!=` rather than `>`, verbatim from `src/WiFiOps.cpp:1198`.
    /// That is what lets a host which has restarted and gone back to epoch 1
    /// still be believed, and it is why re-sending an identical assignment is
    /// acknowledged and then silently discarded.
    fn adopt(&mut self, admin: &AdminMsg) -> bool {
        if admin.assignment_version == self.version {
            return false;
        }
        self.version = admin.assignment_version;
        self.node_index = admin.node_index;
        self.node_count = admin.node_count;
        // Clamped rather than rejected. A range this node cannot walk would
        // strand it: the host has already had a MAC-layer acknowledgement from
        // the radio and believes the assignment landed, so refusing it here
        // would leave the two sides disagreeing with nothing to say so.
        self.end = admin.end_channel_idx.min(NUM_SCAN_CHANNELS - 1);
        self.start = admin.start_channel_idx.min(self.end);
        self.cursor = self.start;
        true
    }

    /// Step to the next channel, saying whether that completed a sweep.
    fn advance(&mut self) -> bool {
        if self.cursor >= self.end {
            self.cursor = self.start;
            return true;
        }
        self.cursor += 1;
        false
    }

    /// The channel the cursor is on.
    fn channel(&self) -> u8 {
        SCAN_CHANNELS[usize::from(self.cursor.min(NUM_SCAN_CHANNELS - 1))]
    }
}

static NODE: StaticCell<Node> = StaticCell::new();

#[cfg(feature = "ble")]
static SCANNER: StaticCell<ble::Scanner<'static>> = StaticCell::new();

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    // Order matters. Configuring the controller needs `&mut`, while `sniffer()`
    // and `esp_now()` each borrow it for as long as they live — so everything
    // mutable happens first and then both handles are held for the rest of the
    // program. That is the whole reason this node sniffs rather than scans:
    // `scan_async` also wants `&mut`, and holding it alongside ESP-NOW is not
    // expressible.
    // `Default::default()` would be China. `esp-radio` defaults `country_info`
    // to `CN` and sets `WIFI_COUNTRY_POLICY_MANUAL`, so the blob applies China's
    // 5 GHz allocation and nothing later overrides it: 36-64 and 149-165 are
    // permitted and the whole of 100-144 is refused. That is exactly the ten
    // channels the C5 refused on the bench, and it is not a DFS rule — 52-64 are
    // DFS too and they work. `US` is the regulatory domain this fleet operates
    // in, and it is the pool the planner already defaults to.
    #[allow(unused_mut, reason = "only the C5 has a band mode to set")]
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

    let mut sniffer = controller.sniffer();
    sniffer.set_receive_cb(sniff::on_frame);
    let (manager, mut sender, receiver) = controller.esp_now().split();

    // Brought up before the loop rather than on demand: initialising a radio
    // between a dwell and an admin window is exactly the kind of surprise this
    // firmware exists to avoid.
    #[cfg(feature = "ble")]
    let mut ble_due = Instant::now();
    #[cfg(feature = "ble")]
    let mut scanner = match ble::Scanner::new(peripherals.BT) {
        Some(scanner) => Some(SCANNER.init(scanner)),
        None => {
            note!("bluetooth controller would not start; scanning Wi-Fi only");
            None
        }
    };

    let node = NODE.init_with(Node::new);
    let mac = esp_radio::wifi::Interface::station().mac_address();
    note!(
        "wartui node {} v{}, control channel {}",
        MacFmt(mac),
        env!("CARGO_PKG_VERSION"),
        CONTROL_CHANNEL
    );

    loop {
        if !node.assigned() {
            // Parked: reachable the whole time, collecting nothing. Heartbeat
            // first and listen afterwards, because the host only ever transmits
            // an assignment in answer to one — so this is how long joining a
            // fleet takes, and there is nothing to be gained by waiting first.
            //
            // Only heartbeat if the radio actually got there, for the reason
            // the sweep gives below. The listen runs either way: it is what
            // keeps this loop from spinning, and a node that cannot tune should
            // still be draining whatever does reach it.
            if radio::park(&manager, &sniffer, CONTROL_CHANNEL, false) {
                heartbeat(&mut sender, node);
            } else {
                note!("radio would not park on channel {}", CONTROL_CHANNEL);
            }
            listen(&receiver, node, IDLE_BEAT_MS);
            continue;
        }

        // Arm after the hop, disarm before the next one. `radio::park` has
        // promiscuous mode on across the channel change, so anything heard
        // inside it belongs to neither channel; a refusal means the radio never
        // arrived, and collecting then would file this channel's name on
        // whatever the radio is actually still tuned to.
        let channel = node.channel();
        if radio::park(&manager, &sniffer, channel, true) {
            sniff::arm(channel);
            CurrentThreadHandle::get().delay(Duration::from_millis(u64::from(CHANNEL_DWELL_MS)));
            sniff::disarm();
        } else {
            note!("radio refused channel {}", channel);
        }

        // Back where the fleet can be heard, for as long as it takes to say
        // what was on that channel. Reporting from anywhere else would be
        // shouting into the room the node was just listening to, so a refusal
        // here costs the observations rather than misdirecting them.
        let on_control = radio::park(&manager, &sniffer, CONTROL_CHANNEL, false);
        if on_control {
            report(&mut sender, node, channel);
            drain_admin(&receiver, node);
        } else {
            note!("radio would not return to channel {}", CONTROL_CHANNEL);
        }

        // `advance` runs either way — a radio that refused one hop must not
        // leave the node dwelling on that channel for ever — but everything
        // else here needs the control channel. A heartbeat sent from a dwell
        // channel is not heard, and worse, it is *counted*: the local transmit
        // succeeds, the counter climbs, and when the node does come back the
        // host sees an unbroken sequence rather than the reboot-shaped gap that
        // would make it re-issue under a fresh epoch. Silence is the honest
        // report of a radio that will not tune.
        if node.advance() && on_control {
            #[cfg(feature = "ble")]
            if Instant::now() >= ble_due {
                ble_due = Instant::now() + Duration::from_millis(BLE_INTERVAL_MS);
                if let Some(scanner) = scanner.as_deref_mut() {
                    report_ble(&mut sender, node, scanner);
                }
            }

            let stagger =
                stagger_offset_ms(node.node_index, node.node_count, NODE_STAGGER_WINDOW_MS);
            if stagger > 0 {
                CurrentThreadHandle::get().delay(Duration::from_millis(u64::from(stagger)));
            }
            heartbeat(&mut sender, node);
            listen(&receiver, node, ADMIN_WAIT_MS);
        }
    }
}

/// Broadcast one heartbeat, which is also the whole of this node's liveness.
///
/// Once per completed sweep, not on a timer (`src/WiFiOps.cpp:1456-1457`). The
/// host reads the period as a rough measure of how many channels the node is
/// carrying, and treats sixty seconds of silence as a node that has left the
/// fleet.
fn heartbeat(sender: &mut EspNowSender<'_>, node: &mut Node) {
    let Ok(msg) = TextMsg::new(MsgType::Heartbeat, node.counter, b"") else { return };
    if radio::broadcast(sender, &msg.encode()) {
        node.counter = node.counter.wrapping_add(1).max(1);
    }
}

/// Report everything heard on `channel` that has not been reported lately.
fn report(sender: &mut EspNowSender<'_>, node: &mut Node, channel: u8) {
    let mut sent = 0u32;
    while let Some(sighting) = sniff::take() {
        if node.seen.contains(&sighting.bssid) {
            continue;
        }
        let mut line = [0u8; WARDRIVE_LINE_MAX];
        let Some(len) = sighting.as_line().write_into(&mut line) else { continue };
        let Ok(msg) = TextMsg::new(MsgType::Text, 0, &line[..len]) else { continue };
        // Recorded only once it is on the air. Suppression is what the ring is
        // for, and suppressing an access point the host never received would
        // hide it for the next two hundred addresses — minutes of a node's
        // life, with nothing anywhere saying why.
        if radio::broadcast(sender, &msg.encode()) {
            node.seen.insert(sighting.bssid);
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

/// Listen for advertisers once per sweep and report the new ones.
///
/// Deliberately here — after the last channel of the sweep, before the
/// heartbeat — so the Bluetooth radio is switched off again well ahead of the
/// window in which this node has to answer an assignment.
#[cfg(feature = "ble")]
fn report_ble(sender: &mut EspNowSender<'_>, node: &mut Node, scanner: &mut ble::Scanner<'_>) {
    let mut lines = 0u32;
    let mut heard = 0u32;
    for report in scanner.sweep() {
        heard += 1;
        if node.seen.contains(&report.address) {
            continue;
        }
        let mut line = [0u8; WARDRIVE_LINE_MAX];
        let Some(len) = report.as_line().write_into(&mut line) else { continue };
        let Ok(msg) = TextMsg::new(MsgType::Text, 0, &line[..len]) else { continue };
        // After the broadcast, for the reason `report` gives.
        if radio::broadcast(sender, &msg.encode()) {
            node.seen.insert(report.address);
            lines += 1;
        }
    }
    if heard > 0 {
        node.reported = node.reported.wrapping_add(lines);
        note!("ble: {} heard, {} new, {} dropped", heard, lines, scanner.dropped());
    }
}

/// Hold the control channel for `ms`, acting on anything that arrives.
///
/// This is the admin window. The radio acknowledges a unicast assignment in
/// hardware whether or not this loop is running, but adopting one needs the
/// frame, and the frame is only read here.
fn listen(receiver: &EspNowReceiver<'_>, node: &mut Node, ms: u32) {
    let until = Instant::now() + Duration::from_millis(u64::from(ms));
    loop {
        drain_admin(receiver, node);
        if Instant::now() >= until {
            return;
        }
        CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
    }
}

/// Take whatever the radio has queued and adopt any assignment in it.
fn drain_admin(receiver: &EspNowReceiver<'_>, node: &mut Node) {
    while let Some(received) = receiver.receive() {
        let Ok(Frame::Admin(admin)) = Frame::decode(received.data()) else { continue };
        if node.adopt(&admin) {
            note!(
                "assigned v{}: indices {}..={} ({}..={}), node {} of {}",
                node.version,
                node.start,
                node.end,
                SCAN_CHANNELS[usize::from(node.start)],
                SCAN_CHANNELS[usize::from(node.end)],
                node.node_index,
                node.node_count
            );
        }
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
