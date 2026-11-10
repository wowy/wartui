//! The fleet engine: a pure synchronous state machine.
//!
//! [`FleetEngine::handle`] reads no clock, touches no socket and opens no file. Inputs arrive as
//! [`Event`]s stamped by the caller, and effects return as an [`ActionBatch`]. So the engine cannot
//! block on the link, and tests run it against a simulated clock.
//!
//! A node is known by its MAC and reached through the node-bus slot its latest heartbeat came
//! from. The P4 decides what each node scans and sends it (`wartui_proto::fleet`). The engine
//! mirrors the plan the P4 reports and reads each heartbeat's epoch against it, so the view shows
//! which nodes hold their share (`scan.rs`).
//!
//! A node counts as gone on the first `Status` that shows its slot absent from the bus, within
//! about a poll of losing power, until its next heartbeat ([`FleetEngine::on_bus`]). The
//! heartbeat timeout is the backstop for a node that still answers the bus but has stopped
//! heartbeating: its bus re-arm runs in an interrupt, so a hung main loop still answers. Both
//! mirror the P4's own membership rule.
//!
//! [`Command::SetBluetooth`] asks the P4 to scan Wi-Fi, with or without Bluetooth, and the P4
//! picks the node that scans it. [`Command::StopScan`] asks it to idle every node.
//! [`Command::SetPool`] asks the bridge to hold a new pool, which its next `Status` brings back.
//! [`Command::SetExcludedSsids`] changes which Wi-Fi networks are never captured.
//! [`Command::PowerCycle`] asks the bridge's carrier to switch one node off and on, and the answer
//! reaches the operator as [`Snapshot::power_answer`].
//! [`Command::SetFanDuty`] sets the carrier's lid fan, whose duty the bridge keeps.
//!
//! `tracing` is the engine's one side channel. It logs each fall behind the bridge and each
//! catch-up, with the pair of bridge stamps that decided it ([`FleetEngine::note_arrival`]). Those
//! stamps are stored nowhere else, so without the log a lag spike cannot be explained. It also
//! logs each node joining, leaving, returning and restarting, so a log alone tells a run's story.

mod config;
mod event;
mod lag;
mod node;
mod rx;
mod scan;
mod snapshot;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::bus::{NodeBoard, Slot, SlotLabel};
use wartui_proto::fleet::{Report, ScanMode, Share};
use wartui_proto::link::{
    BridgeToHost, GPS_BUS_REFUSED, GPS_RECEIVER_ANSWERED, GpsWriteBytes, HostToBridge,
    NO_GPS_RECEIVER,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::plan::{ChannelPool, NUM_SCAN_CHANNELS};

pub use config::{EngineConfig, SEND_TIMEOUT};
pub use event::{ActionBatch, Command, Event, HostSample, Now, StorePeaks, StoreStats};
pub use node::NodeState;
pub use snapshot::{
    BridgeStatus, Counters, NO_CARRIER, NodeView, PowerAnswer, Snapshot, TailEntry,
    power_cycled_text,
};

pub(crate) use lag::BEHIND_US;
pub(crate) use node::supply_dropped_from;
pub(crate) use rx::advance_since_boot;

use crate::distinct::Distinct;
use crate::position::PositionChain;
use crate::record::{BridgeStatusSeen, CaptureSettings, HostStatus, Record};
use crate::ubx;
use rx::FromNode;
use scan::Startup;

/// The fleet's state and the rules that advance it.
#[derive(Debug)]
pub struct FleetEngine {
    config: EngineConfig,
    nodes: BTreeMap<Mac, NodeState>,
    /// Which node each node-bus slot's latest heartbeat named. A sighting batch carries no
    /// address, so this is how it finds its node.
    slots: HashMap<Slot, Mac>,
    bridge: Option<BridgeInfo>,
    /// Whether this bridge life last said no GPS receiver answers on its port
    /// ([`NO_GPS_RECEIVER`], or [`GPS_BUS_REFUSED`]). Cleared when it says the receiver answered
    /// ([`GPS_RECEIVER_ANSWERED`]), and on every connect and disconnect.
    bridge_lacks_gps: bool,
    link_up: bool,
    /// When the link last came up, `None` while down ([`Self::note_departures`]).
    link_up_since: Option<Instant>,
    link_error: Option<String>,
    counters: Counters,
    tail: VecDeque<TailEntry>,
    unique_wifi: Distinct,
    unique_ble: Distinct,
    /// Wi-Fi sightings recorded per entry of [`SCAN_CHANNELS`], indexed like the table, for the
    /// view's channel histogram. Index 13 (channel 14) is never dealt, so it stays 0.
    ///
    /// [`SCAN_CHANNELS`]: wartui_proto::plan::SCAN_CHANNELS
    wifi_by_channel: [u64; NUM_SCAN_CHANNELS as usize],
    bridge_status: Option<BridgeStatus>,
    /// The bridge's drop count when this host attached, subtracted from later readings.
    dropped_baseline: Option<u32>,
    last_status_poll: Option<Instant>,
    started_at_ms: i64,
    /// The channel pool the bridge's last `Status` reported, kept across a disconnect. `None`
    /// until the first.
    pool: Option<ChannelPool>,
    /// The mode the bridge last reported, in a `Status` or a `Fleet` report, kept across a
    /// disconnect like [`Self::pool`]. `None` until the first.
    scan: Option<ScanMode>,
    /// The slot the bridge's planner gave the Bluetooth scan, as its last `Status` reported it,
    /// kept across a disconnect like [`Self::pool`]. `None` for no node, and until the first.
    ble_slot: Option<Slot>,
    /// The bridge's plan, from its last `Fleet` report. `None` on each connection until one
    /// arrives, since a plan is the bridge life's that sent it.
    fleet: Option<Report>,
    /// Each slot's share as the last `Fleet` report of any connection gave it, which
    /// [`Self::record_share_changes`] compares the next report against.
    recorded_shares: BTreeMap<Slot, Share>,
    /// The slots the bridge's last `Status` reported present, a bit per slot ([`Slot::bit`]), kept
    /// across a disconnect like [`Self::pool`]: the next `Status` replaces it. The run's start
    /// waits for a heartbeat from each (`scan.rs`). 0 until the first.
    present: u32,
    /// Where this run stands on starting the scan ([`EngineConfig::auto_start`]).
    startup: Startup,
    /// The scan this run wants, with Bluetooth when `true`: set by every start the engine sends,
    /// and cleared by [`Command::StopScan`]. `None` for an engine that starts nothing, or a fleet
    /// the operator stopped. A P4 reporting stopped while it is `Some` is started again.
    wanted: Option<bool>,
    /// A `Status` has arrived on this connection. The first after a reconnect sets
    /// [`Self::wanted`] from the P4's mode rather than asking for it again.
    status_seen: bool,
    /// The previous frame's bridge stamp and the host instant it was handled, to tell live reading
    /// from a backlog ([`Self::note_arrival`]). `None` until the first frame since start or the
    /// last connect or disconnect.
    last_arrival: Option<(u32, Instant)>,
    /// How far behind the bridge this host is reading, in microseconds. Zero while the link is
    /// read live, climbing while frames arrive faster than wall-clock time allows. Zero on start
    /// and every connect or disconnect ([`Self::note_arrival`] has why).
    backlog_lag_us: u64,
    /// The largest [`Self::backlog_lag_us`] measured since the last [`Event::HostSample`].
    lag_peak_us: u64,
    /// When the current measured spell behind the bridge began. `None` while live. Bounds the log
    /// lines of [`Self::note_arrival`].
    behind_since: Option<Instant>,
    /// The largest lag measured during the spell [`Self::behind_since`] opened.
    behind_peak_us: u64,
    /// The bridge's last answer to a power-cycle.
    power_answer: Option<PowerAnswer>,
}

impl FleetEngine {
    /// Applied host settings for capture provenance. Radio adoption is recorded independently.
    #[must_use]
    pub fn capture_settings(&self) -> CaptureSettings {
        CaptureSettings {
            pool: self.pool,
            mode: self.scan,
            record_raw: self.config.record_raw,
            // The bridge's receiver is built for every run, so it counts only while a bridge is
            // connected.
            gps: self.config.position.gps().is_some()
                || (self.bridge_connected() && self.config.position.bridge_gps().is_some()),
            gps_max_age: self.config.position.max_age(),
            ble_slot: self.ble_slot,
            ble_node: self.ble_slot.and_then(|slot| self.slots.get(&slot).copied()),
        }
    }
    /// Start an engine. `now` fixes the session's start time.
    #[must_use]
    pub fn new(config: EngineConfig, now: Now) -> Self {
        Self {
            nodes: BTreeMap::new(),
            slots: HashMap::new(),
            bridge: None,
            bridge_lacks_gps: false,
            link_up: false,
            link_up_since: None,
            link_error: None,
            counters: Counters::default(),
            tail: VecDeque::new(),
            unique_wifi: Distinct::new(),
            unique_ble: Distinct::new(),
            wifi_by_channel: [0; NUM_SCAN_CHANNELS as usize],
            bridge_status: None,
            dropped_baseline: None,
            last_status_poll: None,
            started_at_ms: now.unix_ms,
            pool: None,
            scan: None,
            ble_slot: None,
            fleet: None,
            recorded_shares: BTreeMap::new(),
            present: 0,
            startup: if config.auto_start {
                Startup::AwaitingStatus { bluetooth: None }
            } else {
                Startup::Done
            },
            wanted: None,
            status_seen: false,
            last_arrival: None,
            // Caught up, as on `Connected`.
            backlog_lag_us: 0,
            lag_peak_us: 0,
            behind_since: None,
            behind_peak_us: 0,
            power_answer: None,
            config,
        }
    }

    /// Advance the state machine. No I/O and no clock reads.
    pub fn handle(&mut self, event: Event, now: Now) -> ActionBatch {
        let mut batch = ActionBatch::default();
        match event {
            Event::Tick => self.on_tick(now, &mut batch),
            Event::Command(command) => self.on_command(command, &mut batch),
            Event::HostSample(sample) => self.on_host_sample(sample, now, &mut batch),
            Event::Link(LinkEvent::Connected(info)) => {
                batch.records.push(Record::Bridge(crate::record::BridgeSeen {
                    mac: info.mac,
                    fw_version: info.fw_version.clone(),
                    node_board: info.node_board,
                }));
                self.bridge = Some(info);
                // A new life. The P4 repeats its GPS fault right after `Ready`.
                self.bridge_lacks_gps = false;
                self.link_up = true;
                self.link_up_since = Some(now.mono);
                self.link_error = None;
                self.forget_bus();
                // Start caught up. Every record from here on was produced after this connection
                // began: the bridge queues nothing for an absent host and clears its queue when one
                // connects, and the transport drops what was already past that queue, which
                // decodes ahead of `Ready` (`wartui_bridge::serial`, `read_loop`).
                self.reset_arrivals();
                // A new connection means a new baseline, whether or not the bridge rebooted.
                self.dropped_baseline = None;
                self.on_connected_scan(&mut batch);
                // Poll now: there is no status yet, and a restarted bridge's GPS receiver has
                // forgotten its rate.
                self.poll_bridge(now, &mut batch);
            }
            Event::Link(LinkEvent::Disconnected { reason }) => {
                self.link_up = false;
                self.bridge_lacks_gps = false;
                self.link_up_since = None;
                self.link_error = Some(reason);
                self.forget_bus();
                self.reset_arrivals();
            }
            Event::Link(LinkEvent::Garbled(_)) => self.counters.garbled += 1,
            Event::Link(LinkEvent::Message(msg)) => self.on_message(&msg, now, &mut batch),
        }
        batch
    }

    /// Record the runtime's sample, this engine's counters and the lag peak, which then resets.
    fn on_host_sample(&mut self, sample: HostSample, now: Now, batch: &mut ActionBatch) {
        let c = &self.counters;
        batch.records.push(Record::HostStatus(HostStatus {
            at_ms: now.unix_ms,
            frames: c.frames,
            duplicate_batches: c.duplicate_batches,
            garbled: c.garbled,
            undecodable: c.undecodable,
            incompatible: c.incompatible,
            lag_peak_us: std::mem::take(&mut self.lag_peak_us),
            store_written: sample.store.written,
            store_dropped: sample.store.dropped,
            store_queue_peak: sample.peaks.queue,
            store_commit_peak_us: sample.peaks.commit_us,
            throttled: sample.health.throttled,
            soc_temp_mc: sample.health.soc_temp_mc,
            battery_mv: sample.health.battery_mv,
            battery_ma: sample.health.battery_ma,
        }));
    }

    fn on_tick(&mut self, now: Now, batch: &mut ActionBatch) {
        self.note_departures(now);
        let due = self
            .last_status_poll
            .is_none_or(|last| now.mono.duration_since(last) >= self.config.status_interval);
        if due && self.link_up {
            self.poll_bridge(now, batch);
        }
        self.resume_start(now, batch);
    }

    /// Forget what the bridge said about the bus: one met again may have restarted, so heartbeats
    /// alone decide until its next `Status`.
    fn forget_bus(&mut self) {
        for node in self.nodes.values_mut() {
            node.off_bus = false;
        }
    }

    /// Log every node that has left the fleet, once, with why: off the bus, or silent past the
    /// topology timeout.
    ///
    /// The P4 has dropped such a node from its plan already. Waits until the link has been up a
    /// whole timeout:
    /// an outage silences every node at once, and until heartbeats flow again after a reconnect,
    /// a live node looks a whole timeout silent.
    fn note_departures(&mut self, now: Now) {
        let settled = self
            .link_up_since
            .is_some_and(|since| now.mono.duration_since(since) >= self.config.topology_timeout);
        if !settled {
            return;
        }
        let gone: Vec<(Mac, Option<SlotLabel>, Option<u64>, &str)> = self
            .nodes
            .values()
            .filter(|node| node.last_heartbeat.is_some() && !node.departed)
            .filter(|node| !self.is_alive(node, now))
            .map(|node| {
                let silent = node.last_heartbeat.map(|last| now.mono.duration_since(last));
                let reason = if self.on_bus(node) { "silent" } else { "off bus" };
                let slot = node.slot.map(|slot| self.slot_label(slot));
                (node.mac, slot, silent.map(|silent| silent.as_millis() as u64), reason)
            })
            .collect();
        for (mac, slot, silent_ms, reason) in gone {
            // An empty `Option` omits the field: a node that lost its slot has none to name.
            let slot = slot.map(tracing::field::display);
            tracing::info!(mac = %mac::full(&mac), slot, reason, silent_ms, "node left the fleet");
            if let Some(node) = self.nodes.get_mut(&mac) {
                node.departed = true;
            }
        }
    }

    /// Ask the bridge how it is.
    ///
    /// A bridge with a GPS receiver is also sent the receiver's rate with every poll, not just the
    /// first. `bulk` drops rather than blocks, so a dropped write has nothing behind it, and the
    /// receiver forgets its configuration whenever the dongle loses power ([`ubx::valset_rate`]
    /// writes RAM only). Rewriting the same values is idempotent, and costs one 64-byte frame per
    /// interval.
    fn poll_bridge(&mut self, now: Now, batch: &mut ActionBatch) {
        self.last_status_poll = Some(now.mono);
        if self.bridge_connected() && self.config.position.bridge_gps().is_some() {
            let bytes = GpsWriteBytes::from_slice(&ubx::valset_rate(self.config.gps_rate_hz))
                .expect("a rate write fits one GpsWrite");
            batch.bulk.push(HostToBridge::GpsWrite { bytes });
        }
        batch.bulk.push(HostToBridge::GetStatus);
    }

    /// What carries the nodes, as the bridge last announced it: no carrier before any has.
    #[must_use]
    pub fn node_board(&self) -> NodeBoard {
        snapshot::announced_board(self.bridge.as_ref())
    }

    /// `slot` as the operator reads it on [`Self::node_board`].
    pub(crate) fn slot_label(&self, slot: Slot) -> SlotLabel {
        self.node_board().label(slot)
    }

    /// Whether a bridge is connected now, and so a GPS port: every P4 reads one. `self.bridge`
    /// outlives a disconnect, so the link must be up too. The runtime reads this after each event
    /// to decide whether the host's search stands down, so the two never disagree about the
    /// bridge.
    #[must_use]
    pub fn bridge_connected(&self) -> bool {
        self.link_up && self.bridge.is_some()
    }

    /// Where the host believes it is: the chain the runtime feeds the bridge's receiver through.
    #[must_use]
    pub const fn position(&self) -> &PositionChain {
        &self.config.position
    }

    fn on_message(&mut self, msg: &BridgeToHost, now: Now, batch: &mut ActionBatch) {
        match msg {
            BridgeToHost::FromNode { slot, rx_us, kind, body } => {
                let rx = FromNode { slot: *slot, rx_us: *rx_us, kind: *kind, body };
                // Before `on_from_node`: heartbeats read `reading_live`.
                self.note_arrival(rx.slot, rx.rx_us, now);
                self.on_from_node(&rx, now, batch);
            }
            BridgeToHost::Fleet(report) => self.on_fleet(report, now, batch),
            BridgeToHost::PowerCycled { slot, outcome } => {
                let text = power_cycled_text(self.node_board(), *slot, *outcome);
                tracing::info!(slot = %self.slot_label(*slot), ?outcome, "{text}");
                let id = self.power_answer.as_ref().map_or(1, |answer| answer.id + 1);
                self.power_answer = Some(PowerAnswer { id, text });
            }
            BridgeToHost::Status {
                rx_count,
                dropped_tx,
                uptime_ms,
                rx_oversize,
                queues,
                radio_link,
                host_uart_errors,
                fan,
                pool,
                scan,
                ble,
                present,
            } => {
                // Below the baseline means the bridge restarted from zero.
                let baseline = match self.dropped_baseline {
                    Some(baseline) if baseline <= *dropped_tx => baseline,
                    _ => *dropped_tx,
                };
                self.dropped_baseline = Some(baseline);
                self.bridge_status = Some(BridgeStatus {
                    rx_count: *rx_count,
                    dropped_tx: *dropped_tx,
                    dropped_since_attach: dropped_tx - baseline,
                    uptime_ms: *uptime_ms,
                    fan: *fan,
                });
                batch.records.push(Record::BridgeStatus(BridgeStatusSeen {
                    rx_at_ms: now.unix_ms,
                    rx_count: *rx_count,
                    dropped_tx: *dropped_tx,
                    uptime_ms: *uptime_ms,
                    host_frames: self.counters.frames,
                    rx_oversize: *rx_oversize,
                    queues: queues.clone(),
                    radio_link: *radio_link,
                    host_uart_errors: *host_uart_errors,
                    fan: *fan,
                    slots_present: *present,
                }));
                // Every heartbeat read so far is no later than this `Status`, so a slot it shows
                // absent holds a node that has not answered since.
                for node in self.nodes.values_mut() {
                    if node.slot.is_some_and(|slot| present & slot.bit() == 0) {
                        node.off_bus = true;
                    }
                }
                self.pool = Some(*pool);
                self.ble_slot = *ble;
                self.present = *present;
                self.on_scan_mode(*scan, scan::Source::Status, now, batch);
                // A node off the bus leaves now, not on the next tick.
                self.note_departures(now);
            }
            // Bridge diagnostics are the transport's to log. The engine reads only those that
            // say whether the bridge has a GPS receiver, for the view.
            BridgeToHost::Error { message }
                if matches!(message.as_str(), NO_GPS_RECEIVER | GPS_BUS_REFUSED) =>
            {
                self.bridge_lacks_gps = true;
            }
            BridgeToHost::Log { message } if message.as_str() == GPS_RECEIVER_ANSWERED => {
                self.bridge_lacks_gps = false;
            }
            // `Ready` arrives as `LinkEvent::Connected`. An update status answers
            // `flash-bridge`, and a cache status and a send's outcome `ota`, which `run` never is.
            // GPS bytes are the runtime's, which feeds them to the position chain before the
            // engine would see them.
            BridgeToHost::Ready { .. }
            | BridgeToHost::Delivered { .. }
            | BridgeToHost::Undelivered { .. }
            | BridgeToHost::Log { .. }
            | BridgeToHost::Error { .. }
            | BridgeToHost::Gps { .. }
            | BridgeToHost::Update(_)
            | BridgeToHost::CacheImage(_) => {}
        }
    }

    /// Whether a node has heartbeated recently enough to hold an assignment, and is on the bus.
    #[must_use]
    pub fn is_alive(&self, node: &NodeState, now: Now) -> bool {
        node.last_heartbeat
            .is_some_and(|last| now.mono.duration_since(last) < self.config.topology_timeout)
            && self.on_bus(node)
    }

    /// Whether a node's slot is on the bus, as far as the bridge has said. False from a `Status`
    /// that shows the node's slot absent until the node's next heartbeat
    /// ([`NodeState::off_bus`]). A later `Status` showing the slot present again is not enough: a
    /// rebooting node, or another node pushed into the socket, answers the bus before any
    /// heartbeat says which node is there and what it holds.
    ///
    /// A `Status` is a priority frame and overtakes bulk node frames, so a heartbeat read after it
    /// is almost always newer. A heartbeat queued behind bulk can still arrive after a `Status`
    /// that already shows its slot absent: the node shows as rejoined until the next `Status`.
    #[must_use]
    pub const fn on_bus(&self, node: &NodeState) -> bool {
        !node.off_bus
    }

    /// Whether a node is one the P4 deals a share to, as far as this host can tell: for the view,
    /// which mirrors the P4's membership rule (`wartui_proto::fleet`).
    ///
    /// Alive is not enough. A node that has not said what it is, or whose firmware does not speak
    /// this build's layouts, heartbeats fine but is never dealt a share. Nor is one whose slot
    /// another node's heartbeat has taken.
    #[must_use]
    pub fn is_assignable(&self, node: &NodeState, now: Now) -> bool {
        self.is_alive(node, now)
            && node.capabilities.is_some_and(|caps| caps.version.compat_with_host().drivable())
            && node.slot.is_some()
    }

    /// What the bridge last said its planner is asked to scan. `None` before it first says.
    #[must_use]
    pub const fn scan(&self) -> Option<ScanMode> {
        self.scan
    }

    /// Whether this engine drives a capture, and so starts the scan
    /// ([`EngineConfig::auto_start`]) and stops it when the run ends.
    #[must_use]
    pub const fn drives_capture(&self) -> bool {
        self.config.auto_start
    }

    /// Running totals, for tests and for the CLI's non-interactive modes.
    #[must_use]
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// The nodes seen so far, ordered by MAC.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeState> {
        self.nodes.values()
    }
}
