//! wartui's wardriving node firmware.
//!
//! A node listens on one channel at a time, reports every access point it has not
//! reported lately, and takes its share of the channel pool from the P4 bridge, which plans
//! the fleet. Its node bus is this node's only link to the bridge, and through it to `wartui`
//! on a laptop.
//!
//! A rewrite rather than a port: the web interface, SD card, display, buttons,
//! fuel gauge, GPS, geofencing, uploads and dock mode did not come across, because
//! every kilobyte of them is a kilobyte that can go wrong where a reflash is the
//! only way to find out.
//!
//! `README.md` § "What it does differently" is the longer account of its design. In
//! short: it *listens* rather than scanning, and sends no Wi-Fi at all; everything it says
//! goes over the node bus ([`bus`]), so it hops from dwell to dwell with no channel to
//! return to; an unassigned node idles rather than sweeping the whole table; the node
//! given the Bluetooth scan sniffs nothing; and every heartbeat says what this build can
//! do. Its messages are wartui's own in both directions — [`wartui_proto::bus`]. It takes
//! a new image over the bus, and boots it on trial until it hears the bridge — [`ota`].
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
use esp_hal::gpio::{Input, InputConfig, Io, Level, Output, OutputConfig, Pull};
use esp_hal::spi::Mode;
use esp_hal::spi::slave::Spi;
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{ControllerConfig, WifiController};
use esp_rtos::CurrentThreadHandle;
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::health;
use wartui_firmware::{reboot, reset_cause};
use wartui_proto::bus::{
    Assign, Board, Capabilities, DecodeError, DieTemp, DownMsg, Heartbeat, PAYLOAD_MAX,
    SIGHTINGS_PER_BATCH_MAX, SightingBatchWriter, SightingMsg, UpType, UpdateState, UpdateStatus,
    Vdd, records,
};
use wartui_proto::dedup::{DEDUP_RSSI_GAIN_BLE_DB, DEDUP_RSSI_GAIN_WIFI_DB};
use wartui_proto::mac::{self, Mac};
use wartui_proto::node::{ASSIGNED_BEAT_MS, CHANNEL_DWELL_MS, IDLE_BEAT_MS, SweepCursor};
use wartui_proto::plan::{ChannelSet, NUM_SCAN_CHANNELS, SCAN_CHANNELS};
use wartui_proto::reset::ResetCause;

mod ble;
mod radio;
mod sniff;

/// The project name in the app descriptor, naming the board as [`BOARD`] does, so an app
/// image says which board it is for.
const PROJECT_NAME: &str = "wartui-node-esp32c5";

// The app descriptor the esp-idf bootloader expects, carrying the board and the version
// from `Cargo.toml`.
esp_bootloader_esp_idf::esp_app_desc!(
    env!("CARGO_PKG_VERSION"),
    PROJECT_NAME,
    esp_bootloader_esp_idf::BUILD_TIME,
    esp_bootloader_esp_idf::BUILD_DATE,
    esp_bootloader_esp_idf::ESP_IDF_COMPATIBLE_VERSION,
    esp_bootloader_esp_idf::MMU_PAGE_SIZE,
    0,
    u16::MAX,
    esp_bootloader_esp_idf::SECURE_VERSION
);

extern crate alloc;

/// Say something on the USB endpoint.
///
/// The bridge is forbidden from printing — its link protocol runs over the same
/// endpoint — but a node has that pipe to itself, so this is the one window into a
/// device with no display, no buttons and no web interface.
///
/// `esp-println`'s serial-JTAG writer gives the FIFO a bounded number of attempts
/// and then remembers that nobody is draining it: losing a line is correct, losing
/// the sweep is not.
macro_rules! note {
    ($($arg:tt)*) => {
        esp_println::println!($($arg)*)
    };
}

mod bus;
mod ota;

/// Granularity of the loops that wait on the bus: fine against the P4's millisecond gap
/// between transfers, coarse enough not to spin the core.
const POLL_MS: u64 = 2;

/// Resets rather than hanging, for the same reason the bridge does: a node that
/// has stopped is indistinguishable from one out of range, and a reset at least
/// restarts the heartbeat counter, which the bridge answers with a fresh assignment
/// and the host shows as `rebooted`.
///
/// The C5 failure [`reboot`] guards against is quieter on this end: a node that never
/// comes back reads as `no heartbeat`, as one out of range does, so it would go
/// unexplained for longer.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    note!("panic: {}", info);
    reboot()
}

// There is no watchdog here, having built one and measured that it cannot work.
// `esp_hal::init` disables every watchdog on the chip
// (`esp-hal-1.2.2/src/lib.rs:768-778`), and the RWDT that would close the gap
// provably never resets these parts: it counts, unfed, and `WDT_PROCPU_RESET_EN`
// is the one enable that will not be written (`docs/phase-3-findings.md` has the
// register dumps, taken on esp-hal 1.1.2).
//
// So a genuine hang in the loop below has nothing behind it and the board has to
// be unplugged: a real gap, left deliberately open, because a safety net that
// provably catches nothing is worse than an absent one. It will be trusted. The gap
// costs a lot on this end, because a hung node reads as `no heartbeat` and so does
// one out of range or with a flat battery.

/// Everything that changes while the node runs.
///
/// Built at compile time into a [`ConstStaticCell`] and taken as a `&'static mut`,
/// rather than built in a local and moved in: nothing outside the main loop touches
/// it, so it needs no lock — unlike the dedup ring, which lives in [`sniff::SEEN`]
/// instead because the receive callback has to reach it from another task.
struct Node {
    /// This node's station MAC, the address the host knows it by.
    mac: Mac,
    /// The epoch of the assignment held. Zero means none is held, since boot or the
    /// last stop; no assignment carries it on the wire. Sent back in every heartbeat,
    /// so the bridge and the host can tell adoption from a bus ack.
    version: u8,
    /// Which [`SCAN_CHANNELS`] indices to dwell on, in ascending order.
    channels: ChannelSet,
    /// Whether the bridge asked this node to scan Bluetooth.
    ble: bool,
    /// Where the sweep has got to in those indices.
    cursor: SweepCursor,
    /// Monotonic from boot, counting completed sweeps (a Bluetooth node's
    /// completed scans, an idle node's beats). Its going backwards is how the
    /// bridge and the host notice a node has restarted and forgotten its assignment.
    counter: u32,
    /// When an assigned node next sends a heartbeat. An unassigned node heartbeats
    /// every [`IDLE_BEAT_MS`] instead, with no deadline of its own. Set fresh every
    /// time an assignment is adopted while idle, not only the first, so a stale
    /// deadline from before this node last went idle never fires early. `None`, on a
    /// node assigned some other way, is due at once.
    next_beat: Option<Instant>,
    reported: u32,
    /// This sighting batch's sequence number, advanced only once the P4
    /// acknowledges a batch. A host-visible gap in it is loss after the P4 took
    /// the batch (its queue or USB path), never loss on the bus: an
    /// unacknowledged batch reuses `seq` rather than advancing past it.
    seq: u16,
    /// Sighting batches this node sent that the P4 never acknowledged.
    unacked: u32,
    /// Why this boot happened, read once in `main`. Every heartbeat carries it.
    reset_cause: ResetCause,
    /// Whether this image is on trial, not yet confirmed by a record from the bridge. Every
    /// heartbeat carries it as [`Capabilities::trial`]; `ota` has the rest.
    trial: bool,
}

impl Node {
    // Only ever evaluated at compile time, into `NODE`, so the frame this lint
    // measures is never on a stack.
    #[allow(clippy::large_stack_frames)]
    const fn new() -> Self {
        Self {
            mac: [0; 6],
            version: 0,
            channels: ChannelSet::empty(),
            ble: false,
            cursor: SweepCursor::new(),
            counter: 1,
            next_beat: None,
            reported: 0,
            seq: 0,
            unacked: 0,
            // `main` replaces it before the first heartbeat.
            reset_cause: ResetCause::Unknown,
            trial: false,
        }
    }

    /// Whether the node has anything to do, given whether it has a scan to run.
    ///
    /// Channels, or the Bluetooth scan. An empty mask with the flag clear counts as
    /// nothing rather than as an error: no assignment means "scan nothing", so this needs a
    /// confused planner, and idling is the same answer as never having been told anything.
    const fn assigned(&self, scanning: bool) -> bool {
        self.version != 0 && (!self.channels.is_empty() || self.bluetooth_only(scanning))
    }

    /// Whether this node's whole job is Bluetooth.
    ///
    /// `scanning` is whether the controller started. The bridge cannot see that —
    /// every node can scan, so capabilities carry no Bluetooth bit — so this node
    /// does not depend on the bridge never asking: a flag that reaches one with no
    /// controller leaves it idle and reporting nothing, honest and diagnosable
    /// from the adoption line.
    const fn bluetooth_only(&self, scanning: bool) -> bool {
        scanning && self.ble && self.channels.is_empty()
    }

    /// Take an assignment, if it is not the one already held.
    ///
    /// The test is `!=` rather than `>`, so a bridge that restarted and went back to
    /// epoch 1 is still believed — and so re-sending an identical assignment is
    /// acknowledged and silently discarded.
    ///
    /// Nothing is rejected: `ChannelSet` drops bits above [`NUM_SCAN_CHANNELS`]
    /// rather than refusing the record, because the bus has already acknowledged
    /// it and a node that quietly declined would leave the two sides disagreeing
    /// with nothing to say so.
    ///
    /// Forgets every reported address first when the new share changes what this
    /// node scans or its Bluetooth flag: entries built under the old share describe
    /// a neighborhood this node no longer listens to. A first assignment after
    /// boot finds the ring already empty, so that case costs nothing.
    fn adopt(&mut self, assign: &Assign) -> bool {
        if assign.epoch == self.version {
            return false;
        }
        if assign.channels != self.channels || assign.scan_ble() != self.ble {
            self.forget_reported("new share");
        }
        self.version = assign.epoch;
        self.channels = assign.channels;
        self.ble = assign.scan_ble();
        self.cursor = SweepCursor::new();
        true
    }

    /// Drop the assignment and forget every reported address, returning to the state this
    /// node boots in: [`Self::assigned`] is false, so the main loop idles, and heartbeats
    /// carry epoch 0 at the idle rate. Any later assignment is adopted, whatever its epoch,
    /// since none carries 0.
    ///
    /// A dwell or Bluetooth scan in progress runs out first, as it does when an assignment
    /// changes: the record is acted on in the drain after it.
    fn stop(&mut self) {
        self.forget_reported("stopped");
        self.version = 0;
        self.channels = ChannelSet::empty();
        self.ble = false;
        self.cursor = SweepCursor::new();
    }

    /// Empty the dedup ring, so the next sweep reports this node's whole
    /// neighborhood again.
    ///
    /// The one way the ring gets emptied: anything else that should empty it —
    /// an adopted share change, a stop — calls this rather than
    /// touching [`sniff::SEEN`] directly.
    fn forget_reported(&mut self, why: &str) {
        let len = sniff::SEEN.with(|seen| {
            let len = seen.len();
            seen.clear();
            len
        });
        note!("forgot {} reported addresses: {}", len, why);
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

/// The board this build is for.
const BOARD: Board = Board::Esp32c5;

/// What this node is, as every heartbeat says it: `NODE_VERSION`, the board and its flags.
///
/// Bluetooth is not a capability at all: every node can scan it, and the scan is the
/// bridge's decision, off at every boot.
const CAPABILITIES: Capabilities = Capabilities::here(BOARD);

static NODE: ConstStaticCell<Node> = ConstStaticCell::new(Node::new());

static SCANNER: StaticCell<ble::Scanner<'static>> = StaticCell::new();

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let cause = reset_cause();
    note!("reset reason: {:?} ({})", esp_hal::system::reset_reason(), cause);

    // Before the radio, so an image that has run out of trial boots is rolled back before
    // it does anything else.
    let mut ota = ota::Ota::new(peripherals.FLASH);
    let trial = ota.boot();

    // The radio blobs allocate; nothing in wartui's own code does.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // The scheduler has to be running before the radio comes up.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // The controller is configured before `sniffer()` hands out its handle, so the first
    // frame it sees is under the band set here. The handle does not borrow the controller:
    // it holds a reference that keeps Wi-Fi running, and both are held for the rest of the
    // program. The controller sets the channel (`radio::park`). `README.md` § "What it does
    // differently" has why this node sniffs rather than scans.
    //
    // `US` rather than `esp-radio`'s default, China under `WIFI_COUNTRY_POLICY_MANUAL`,
    // which refuses 5 GHz 100-144 outright and so silently costs a C5 those channels on
    // every sweep.
    let mut controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_country_info(*b"US"),
    )
    .expect("Wi-Fi controller");

    // Without this, every 5 GHz channel in the pool is refused.
    match controller.set_band_mode(esp_radio::wifi::BandMode::Auto) {
        Ok(()) => {}
        Err(err) => note!("could not enable dual band: {:?}", err),
    }

    // The node sends no Wi-Fi frames of its own. The cap holds anything the MAC answers in
    // hardware, such as an ack to a frame addressed to this station, at 2 dBm, the lowest power
    // `set_max_tx_power` accepts, in ESP-IDF's quarter-dBm units.
    const MIN_TX_POWER_QUARTER_DBM: i8 = 8;
    match controller.set_max_tx_power(MIN_TX_POWER_QUARTER_DBM) {
        Ok(()) => {}
        Err(err) => note!("could not cap transmit power: {:?}", err),
    }

    let mut sniffer = controller.sniffer();
    sniffer.set_receive_cb(sniff::on_frame);

    // Brought up before the loop rather than on demand: initializing a radio
    // between two dwells is the kind of surprise to avoid.
    let mut scanner = match ble::Scanner::new(peripherals.BT) {
        Some(scanner) => Some(SCANNER.init(scanner)),
        None => {
            // The bridge cannot see this: capabilities carry no Bluetooth bit, so a
            // controller that would not start is a fact only this node's own
            // console knows. It sniffs Wi-Fi as normal, and idles if handed the
            // scan anyway.
            note!("bluetooth controller would not start; sniffing Wi-Fi only");
            None
        }
    };
    let scanning = scanner.is_some();

    // The node bus, on the XIAO's D7-D10 and D0 (`docs/node-bus.md`). Pulled up, so CS reads
    // idle with no P4 attached. One pin, two readers: the SPI driver takes it as CS, and
    // `bus` re-arms on its rising edge.
    let cs = Input::new(peripherals.GPIO12, InputConfig::default().with_pull(Pull::Up));
    let spi = Spi::new(peripherals.SPI2, Mode::_0)
        .with_sck(peripherals.GPIO8)
        .with_mosi(peripherals.GPIO10)
        .with_miso(peripherals.GPIO9)
        .with_cs(cs.peripheral_input())
        .with_dma(peripherals.DMA_CH0);
    let irq = Output::new(peripherals.GPIO1, Level::Low, OutputConfig::default());
    let mut io = Io::new(peripherals.IO_MUX);
    bus::start(spi, cs, irq, &mut io);

    let node = NODE.take();
    node.reset_cause = cause;
    node.trial = trial;
    node.mac = esp_radio::wifi::Interface::station().mac_address();
    note!("wartui node {} v{}", mac::full(&node.mac), env!("CARGO_PKG_VERSION"));

    loop {
        drain(node, &mut ota);
        if ota.updating() {
            // Update mode: sniffing nothing and scanning no Bluetooth, servicing the bus
            // throughout. It heartbeats every second whatever the assignment, so the host
            // sees it alive, and keeps that assignment to return to.
            heartbeat(node);
            listen(node, &mut ota, scanning, IDLE_BEAT_MS);
            ota.expire();
            continue;
        }

        if !node.assigned(scanning) {
            // Idle: servicing the bus the whole time, collecting nothing. Heartbeat first
            // and listen afterwards, because a scanning bridge sends an assignment in
            // answer to one, so this is how long joining a fleet takes. One idle beat is
            // this node's whole cycle, so the counter the heartbeat carries advances here
            // too.
            heartbeat(node);
            node.counter = node.counter.wrapping_add(1).max(1);
            listen(node, &mut ota, scanning, IDLE_BEAT_MS);
            // An assignment adopted mid-listen: this node has just heartbeated, so its
            // next one is a whole interval out from here rather than immediate. Set every
            // time, since a stale deadline from before this node last went idle would
            // otherwise fire early.
            if node.assigned(scanning) {
                node.next_beat =
                    Some(Instant::now() + Duration::from_millis(u64::from(ASSIGNED_BEAT_MS)));
            }
            continue;
        }

        if node.bluetooth_only(scanning) {
            // Bluetooth is this node's whole job, so it sniffs nothing and the Bluetooth
            // controller has the shared 2.4 GHz antenna to itself.
            //
            // One scan is this node's whole sweep, so the counter advances every
            // time regardless of whether a heartbeat goes out on this pass.
            //
            // Always `Some` here: `scanning` is what put this node in this arm, and it is
            // that scanner being there.
            if let Some(scanner) = scanner.as_deref_mut() {
                report_ble(node, scanner);
            }
            node.counter = node.counter.wrapping_add(1).max(1);
            // Drain first, so a heartbeat due now reports an epoch adopted since the scan.
            drain(node, &mut ota);
            beat_if_due(node);
            // Straight into the next scan: a beat not yet due costs this node nothing but
            // the check.
            continue;
        }

        // Arm after the hop, disarm before the next one. `radio::park` has
        // promiscuous mode on across the channel change, so anything heard inside
        // it belongs to neither channel — and a refusal means the radio never
        // arrived, so collecting then would file this channel's name on whatever
        // it is still tuned to.
        let Some(channel) = node.channel() else {
            // An assignment adopted while idle, whose sweep has not begun: step onto
            // its lowest channel rather than whichever index the cursor held when the
            // record arrived, which may no longer be assigned.
            node.advance();
            continue;
        };
        if radio::park(&mut controller, &sniffer, channel) {
            sniff::arm(channel);
            CurrentThreadHandle::get().delay(Duration::from_millis(u64::from(CHANNEL_DWELL_MS)));
            sniff::disarm();
            radio::quiet(&sniffer);
            report(node, channel);
        } else {
            note!("radio refused channel {}", channel);
        }

        // `advance` runs either way, so a radio that refused one hop does not leave
        // the node dwelling there forever, and the counter it bumps on a completed
        // sweep counts every one of them, whether or not a heartbeat goes out on
        // this pass. Drained first, so a heartbeat due now reports an epoch adopted
        // since the last pass.
        drain(node, &mut ota);
        if node.advance() {
            node.counter = node.counter.wrapping_add(1).max(1);
        }
        beat_if_due(node);
    }
}

/// Send a heartbeat if an assigned node's deadline has passed, and set the next one.
fn beat_if_due(node: &mut Node) {
    let now = Instant::now();
    let deadline = node.next_beat.unwrap_or(now);
    if now >= deadline {
        heartbeat(node);
        node.next_beat = Some(next_beat_after(deadline));
    }
}

/// Push a heartbeat deadline forward by one [`ASSIGNED_BEAT_MS`], never behind now.
///
/// A node that stalled — a long run of refused hops, a wedged radio recovering —
/// must not fire a burst of overdue beats once it catches up: the next one is
/// always a whole interval away from whichever moment noticed the deadline had
/// passed, rather than from the deadline itself.
fn next_beat_after(deadline: Instant) -> Instant {
    let next = deadline + Duration::from_millis(u64::from(ASSIGNED_BEAT_MS));
    let now = Instant::now();
    if next <= now { now + Duration::from_millis(u64::from(ASSIGNED_BEAT_MS)) } else { next }
}

/// Send one heartbeat over the bus, which is also the whole of this node's liveness.
///
/// At most once every [`ASSIGNED_BEAT_MS`] for an assigned node. The counter it carries
/// tracks completed sweeps on its own timer, so it advances whether or not a beat reaches
/// the host; the bridge and the host treat 20 s of silence as a node that has left the fleet.
/// `node.version` is the epoch this node holds, which is how the bridge and the host tell
/// adoption from a bus ack. Fire-and-forget: a newer heartbeat replaces one the P4 has not clocked
/// (`bus::send`). The rail voltage, die temperature and free heap are read between dwells, so
/// the voltage read's PBUS excursion never lands in one.
fn heartbeat(node: &Node) {
    // Every heartbeat carries the capabilities, not just the first: sent once they
    // would be lost to a dropped beat or stale after a reflash, and they are
    // five bytes of twenty-seven.
    let msg = Heartbeat {
        capabilities: Capabilities { trial: node.trial, ..CAPABILITIES },
        mac: node.mac,
        counter: node.counter,
        epoch: node.version,
        wifi_refused: sniff::refused(),
        ble_refused: ble::refused(),
        reset_cause: node.reset_cause,
        vdd: Vdd::from_mv(health::vdd_mv()),
        die_temp: die_temp(health::die_temp_c()),
        heap_free: esp_alloc::HEAP.free() as u32,
    };
    bus::send(UpType::Heartbeat, &msg.encode());
}

/// Encode a die reading, sending a failed one as no reading.
fn die_temp(c: i16) -> DieTemp {
    // The PHY floors a failed read at −200 °C, and no running die is below −40 °C.
    if c < -40 { DieTemp::NONE } else { DieTemp::from_c(c) }
}

/// Packs every sighting one dwell or Bluetooth scan produces into as few batches as
/// `SIGHTING_BATCH_MAX` (1024 bytes) allows, delivers each over the bus, and sends what
/// it holds at the end rather than carrying anything into the next one — the host
/// stamps a sighting's position on arrival, so held sightings would carry the
/// wrong one.
///
/// `members` mirrors what `writer` currently holds, so an acknowledged batch can be
/// turned into a dedup-ring entry per record without re-decoding the batch just sent.
///
/// `failed` latches once a batch in this report goes unacknowledged: every batch after
/// it in the same report is discarded rather than packed and retried. With no P4
/// clocking this node nothing it sends is ever acknowledged, so without the latch every
/// dwell would end in a string of batches, each waiting out its timeouts. One failed
/// batch is the bound instead; nothing discarded enters `SEEN`, so it stays due, and the
/// next report's first batch is the probe that finds out whether the P4 has come back —
/// recovery needs no extra logic.
struct Outgoing {
    writer: SightingBatchWriter,
    members: [([u8; 6], Option<i8>); SIGHTINGS_PER_BATCH_MAX],
    len: usize,
    failed: bool,
}

impl Outgoing {
    fn new(seq: u16) -> Self {
        Self {
            writer: SightingBatchWriter::new(seq),
            members: [([0u8; 6], None); SIGHTINGS_PER_BATCH_MAX],
            len: 0,
            failed: false,
        }
    }

    /// Add one sighting, flushing first if it does not fit what is already
    /// packed. Returns how many records a flush this triggered actually sent,
    /// which is zero unless it had to make room.
    ///
    /// Once `failed` is set, this stops packing anything: `report`/`report_ble`
    /// still call it for every address so their buffers drain, but it discards
    /// rather than filling a batch nobody would acknowledge. The address is never
    /// entered in `SEEN`, so it is due again next time it is heard.
    fn offer(
        &mut self,
        node: &mut Node,
        msg: &SightingMsg<'_>,
        addr: [u8; 6],
        rssi: Option<i8>,
        now: u32,
    ) -> u32 {
        if self.failed {
            return 0;
        }
        if self.writer.push(msg) {
            self.remember(addr, rssi);
            return 0;
        }
        let flushed = self.flush(node, now);
        // A flush that just failed leaves nothing to pack this sighting behind: taking
        // it would hand the final flush a second batch for an absent P4.
        if self.failed {
            return flushed;
        }
        // An empty batch has room for any record that encodes at all, so a refusal
        // here is a record over its limits: skipped, and never entered in `SEEN`.
        if self.writer.push(msg) {
            self.remember(addr, rssi);
        }
        flushed
    }

    /// Note an address the writer has just taken, for [`Self::flush`] to record.
    fn remember(&mut self, addr: [u8; 6], rssi: Option<i8>) {
        if self.len < self.members.len() {
            self.members[self.len] = (addr, rssi);
            self.len += 1;
        }
    }

    /// Deliver what is packed, if anything, returning how many records it held. `SEEN`
    /// is only updated once the P4 acknowledges the batch, so a batch nobody
    /// acknowledged stays due to be offered again next time its addresses are heard.
    fn flush(&mut self, node: &mut Node, now: u32) -> u32 {
        if self.writer.is_empty() {
            return 0;
        }
        let sent = if bus::deliver(UpType::Sightings, self.writer.as_bytes()) {
            for &(addr, rssi) in &self.members[..self.len] {
                sniff::SEEN.with(|seen| seen.record(addr, rssi, now));
            }
            node.seq = node.seq.wrapping_add(1);
            u32::try_from(self.len).unwrap_or(u32::MAX)
        } else {
            node.unacked = node.unacked.wrapping_add(1);
            self.failed = true;
            0
        };
        self.writer.reset(node.seq);
        self.len = 0;
        sent
    }
}

/// Report everything heard on `channel` that has not been reported lately.
fn report(node: &mut Node, channel: u8) {
    let now = sniff::now_ms();
    let mut outgoing = Outgoing::new(node.seq);
    let mut sent = 0u32;
    while let Some(sighting) = sniff::take() {
        let rssi = Some(sighting.rssi);
        // Taken and released per sighting, never across the delivery inside
        // `offer` — the callback's own rule for `sniff::SEEN`, kept here too.
        if !sniff::SEEN
            .with(|seen| seen.is_due(&sighting.bssid, rssi, DEDUP_RSSI_GAIN_WIFI_DB, now))
        {
            continue;
        }
        sent += outgoing.offer(node, &sighting.as_msg(), sighting.bssid, rssi, now);
    }
    sent += outgoing.flush(node, now);
    if sent > 0 || outgoing.failed {
        node.reported = node.reported.wrapping_add(sent);
        note!(
            "ch {}: {} new, {} total, {} refused, {} unacked",
            channel,
            sent,
            node.reported,
            sniff::refused(),
            node.unacked
        );
    }
}

/// Listen for advertisers and report the new ones.
///
/// Called once per scan on the node whose whole job this is. The node sniffs no
/// Wi-Fi, so the scan competes with nothing for the antenna.
fn report_ble(node: &mut Node, scanner: &mut ble::Scanner<'_>) {
    let heard = scanner.sweep();
    let now = sniff::now_ms();
    let mut outgoing = Outgoing::new(node.seq);
    let mut lines = 0u32;
    // One report at a time out of the buffer, so the lock is never held across a
    // delivery — the same shape the Wi-Fi sightings are drained in.
    while let Some(report) = ble::take() {
        let rssi = report.has_rssi().then_some(report.rssi);
        if !sniff::SEEN.with(|seen| seen.is_due(&report.address, rssi, DEDUP_RSSI_GAIN_BLE_DB, now))
        {
            continue;
        }
        lines += report.with_msg(|msg| outgoing.offer(node, &msg, report.address, rssi, now));
    }
    lines += outgoing.flush(node, now);
    if heard > 0 || outgoing.failed {
        node.reported = node.reported.wrapping_add(lines);
        note!(
            "ble: {} heard, {} new, {} refused, {} unacked",
            heard,
            lines,
            ble::refused(),
            node.unacked
        );
    }
}

/// Service the bus for `ms`, acting on whatever the P4 sends.
///
/// Returns early when update mode starts or ends, or the node gains or loses an
/// assignment, so the main loop changes mode at once.
fn listen(node: &mut Node, ota: &mut ota::Ota, scanning: bool, ms: u32) {
    let until = Instant::now() + Duration::from_millis(u64::from(ms));
    let updating = ota.updating();
    let assigned = node.assigned(scanning);
    loop {
        drain(node, ota);
        if Instant::now() >= until
            || ota.updating() != updating
            || node.assigned(scanning) != assigned
        {
            return;
        }
        CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
    }
}

/// Service the bus and act on every record the P4 has sent: adopt an assignment, stop,
/// and hand update records to `ota`.
///
/// Any record from the P4 this node decodes confirms an image on trial — assign, stop,
/// update confirm or an update record: decoding one is hearing the bridge over the bus,
/// which is the whole test.
fn drain(node: &mut Node, ota: &mut ota::Ota) {
    bus::service();
    let mut payload = [0u8; PAYLOAD_MAX];
    while let Some(len) = bus::take(&mut payload) {
        for record in records(&payload[..len]) {
            match DownMsg::decode(record.kind, record.body) {
                Ok(DownMsg::Assign(assign)) => {
                    confirm(node, ota);
                    if node.adopt(&assign) {
                        note!(
                            "assigned v{}: {} channels ({}), ble {}",
                            node.version,
                            node.channels.len(),
                            Channels(node.channels),
                            if node.ble { "on" } else { "off" }
                        );
                    }
                }
                Ok(DownMsg::Stop(_)) => {
                    confirm(node, ota);
                    node.stop();
                }
                Ok(DownMsg::UpdateConfirm(_)) => confirm(node, ota),
                Ok(DownMsg::UpdateBegin(msg)) => update(node, ota, &ota::Update::Begin(msg)),
                Ok(DownMsg::UpdateChunk(msg)) => update(node, ota, &ota::Update::Chunk(msg)),
                Ok(DownMsg::UpdateCommit(msg)) => update(node, ota, &ota::Update::Commit(msg)),
                Ok(DownMsg::UpdateAbort(msg)) => update(node, ota, &ota::Update::Abort(msg)),
                // A host whose record is not the shape this build reads: a type it does
                // not know, or a different length. Said out loud rather than dropped: it
                // is the whole diagnosis for a node that is talked to and never answers.
                Err(DecodeError::BadLength { need, got }) => note!(
                    "ignoring a {} byte type {} record, this build reads {}; update this node",
                    got,
                    record.kind,
                    need
                ),
                Err(DecodeError::UnknownType(kind)) => {
                    note!("ignoring a record of unknown type {}; update this node", kind);
                }
                Err(err) => note!("ignoring a type {} record: {}", record.kind, err),
            }
        }
    }
}

/// End this image's trial, if it is on one. Once it takes, it is a no-op for the rest of
/// the boot.
fn confirm(node: &mut Node, ota: &mut ota::Ota) {
    if node.trial && ota.confirm() {
        node.trial = false;
    }
}

/// Act on an update record, confirming a trial image first.
///
/// Confirming first matters: an update accepted while this image is on trial would write
/// over the only slot known to work, so `ota` refuses a `begin` until the trial is over.
///
/// A `Verified` status is followed by the switch to the new image and a reboot. The
/// host also learns of the update from the reboot. A switch that fails sends one
/// `FlashError` instead.
fn update(node: &mut Node, ota: &mut ota::Ota, record: &ota::Update<'_>) {
    confirm(node, ota);
    let Some(status) = ota.on_update(record, node.trial) else { return };
    send_status(&status);
    if status.state == UpdateState::Verified {
        send_status(&ota.install());
    }
}

/// Deliver an update status to the P4, noting one it never acknowledged.
fn send_status(status: &UpdateStatus) {
    if !bus::deliver(UpType::UpdateStatus, &status.encode()) {
        note!("update status {:?} unacknowledged", status.state);
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
