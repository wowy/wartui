//! The fleet engine: a pure synchronous state machine.
//!
//! [`FleetEngine::handle`] reads no clock, touches no socket and opens no file. Inputs arrive as
//! [`Event`]s stamped by the caller, and effects return as an [`ActionBatch`]. So the engine cannot
//! block on the link, and tests run it against a simulated clock.
//!
//! A node is known by its MAC and reached through the node-bus slot its latest heartbeat came
//! from. Sending it anything means allocating an epoch, answering its heartbeat, and counting the
//! record delivered only when the bridge says the node acknowledged it. The engine
//! partitions the channel pool across live, heartbeating nodes, re-cutting when membership or the
//! pool changes. Only the planner's re-cut decides what a node scans. The pool is the bridge's:
//! the engine learns it from each `Status`, and cuts nothing before the first.
//!
//! A node leaves the plan on the first `Status` that shows its slot absent from the bus, within
//! about a poll of losing power, and stays out until its next heartbeat ([`FleetEngine::on_bus`]).
//! The heartbeat timeout is the backstop for a node that still answers the bus but has stopped
//! heartbeating: its bus re-arm runs in an interrupt, so a hung main loop still answers.
//!
//! [`Command::AssignBle`] asks the bridge to give one slot's node the Bluetooth scan as its whole
//! job. The bridge holds that slot as it holds the pool, and the engine gives the scan to whichever
//! assignable node heartbeats from it.
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

mod admin;
mod config;
mod event;
mod lag;
mod node;
mod replan;
mod rx;
mod snapshot;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::bus::{NodeBoard, Slot, SlotLabel};
use wartui_proto::link::{
    BridgeToHost, GPS_BUS_REFUSED, GPS_RECEIVER_ANSWERED, GpsWriteBytes, HostToBridge,
    NO_GPS_RECEIVER,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::plan::{ChannelPool, Job, NUM_SCAN_CHANNELS, Plan};

pub use config::{EngineConfig, SEND_TIMEOUT};
pub use event::{ActionBatch, Command, Event, HostSample, Now, StorePeaks, StoreStats};
pub use node::{Assignment, NodeState};
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
use admin::Pending;
#[cfg(doc)]
use replan::next_epoch;
use rx::FromNode;

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
    /// Assignments in flight, keyed by the id the bridge will echo back.
    pending: BTreeMap<u16, Pending>,
    /// Wraps harmlessly: an id need only be unique among the few sends in flight.
    next_send_id: u16,
    /// The last epoch handed out. Starts at 0 each capture and is not persisted. A node may hold an
    /// epoch from an earlier run, so [`next_epoch`] skips the one its heartbeat reports.
    last_counter: u64,
    /// The channel pool the bridge's last `Status` reported, kept across a disconnect. `None`
    /// until the first, and nothing is cut until then.
    pool: Option<ChannelPool>,
    /// The partition in force.
    plan: Option<Plan>,
    /// The members the plan was cut for, in slot order, each with its job. [`Self::replan`]
    /// re-partitions when the live membership differs, job included: a node that took the
    /// Bluetooth scan holds a share of the wrong shape.
    plan_members: Vec<(Mac, Job)>,
    /// The slot whose node scans Bluetooth, as the bridge's last `Status` reported it, kept across
    /// a disconnect like [`Self::pool`]. `None` for no node, and until the first `Status`.
    ble_slot: Option<Slot>,
    /// The one node asked to scan Bluetooth, if any: the assignable node heartbeating from
    /// [`Self::ble_slot`], derived at the top of every [`Self::replan`].
    ///
    /// Beside the plan, not in it: the slot is an operator's choice the bridge holds, while the
    /// plan follows who is present. It reaches the planner as a [`Job::Bluetooth`] slot on every
    /// re-cut, which deals its Wi-Fi share to the rest.
    ble_node: Option<Mac>,
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
            record_raw: self.config.record_raw,
            // The bridge's receiver is built for every run, so it counts only while a bridge is
            // connected.
            gps: self.config.position.gps().is_some()
                || (self.bridge_connected() && self.config.position.bridge_gps().is_some()),
            gps_max_age: self.config.position.max_age(),
            ble_slot: self.ble_slot,
            ble_node: self.ble_node,
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
            pending: BTreeMap::new(),
            next_send_id: 1,
            last_counter: 0,
            pool: None,
            plan: None,
            plan_members: Vec::new(),
            ble_slot: None,
            ble_node: None,
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
        self.expire_pending(now, batch);
        self.follow_departures(now);
        let due = self
            .last_status_poll
            .is_none_or(|last| now.mono.duration_since(last) >= self.config.status_interval);
        if due && self.link_up {
            self.poll_bridge(now, batch);
        }
    }

    /// Act on the nodes that have left the fleet: log each once, and re-cut the plan without them.
    /// The re-cut also takes the Bluetooth scan from one that held it, since it is no longer
    /// assignable. A node leaves on a tick, past the heartbeat timeout, or on a `Status` that shows
    /// its slot absent, and both call this.
    fn follow_departures(&mut self, now: Now) {
        self.note_departures(now);
        self.replan(now);
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
    /// Such a node is out of the plan already. Waits until the link has been up a whole timeout:
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
            BridgeToHost::Delivered { id } => self.on_send_result(*id, None, now, batch),
            BridgeToHost::Undelivered { id, reason } => {
                self.on_send_result(*id, Some(*reason), now, batch);
            }
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
                self.on_bridge_pool(*pool);
                self.on_bridge_ble(*ble);
                // A node off the bus leaves now, not on the next tick. The re-cut also moves the
                // Bluetooth scan to the slot just reported.
                self.follow_departures(now);
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
            // `flash-bridge`, and a cache status `ota`, which `run` never is. GPS bytes are the
            // runtime's, which feeds them to the position chain before the engine would see them.
            BridgeToHost::Ready { .. }
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
    /// that already shows its slot absent: the node rejoins until the next `Status`, at the cost
    /// of one extra re-cut.
    #[must_use]
    pub const fn on_bus(&self, node: &NodeState) -> bool {
        !node.off_bus
    }

    /// Whether a node can be given channels.
    ///
    /// Alive is not enough. A node that has not said what it is, or whose firmware does not speak
    /// this build's layouts, heartbeats fine but never scans its share. Nor does one whose slot
    /// another node's heartbeat has taken, which this host cannot reach. A share nobody scans is worse than one node fewer: the fleet covers less of
    /// the pool than without it.
    #[must_use]
    pub fn is_assignable(&self, node: &NodeState, now: Now) -> bool {
        self.is_alive(node, now)
            && node.capabilities.is_some_and(|caps| caps.version.compat_with_host().drivable())
            && node.slot.is_some()
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
