//! The fleet engine: a pure synchronous state machine.
//!
//! # Architecture
//!
//! [`FleetEngine::handle`] reads no clock, touches no socket, and opens no file.
//! Inputs arrive as an [`Event`] with caller-determined timestamps, and all effects
//! are emitted as an [`ActionBatch`] to prevent accidental link blocking and ensure
//! deterministic testing against simulated clocks.
//!
//! # Transmission & Partitioning
//!
//! Transmitting involves allocating an epoch, waiting for the heartbeat that opens a node's
//! 100 ms admin window, and confirming delivery strictly via MAC-layer acknowledgement.
//! The engine maintains a partitioned channel pool across all active, heartbeating nodes and
//! re-cuts the allocation whenever membership changes or the operator changes the pool; no logic
//! outside the planner's re-cut dictates node scanning behavior.
//!
//! # Fleet Commands
//!
//! - [`Command::AssignBle`]: Designates a single node as the dedicated Bluetooth scanner,
//!   allocating it zero Wi-Fi channels.
//! - [`Command::SetTxPower`]: Updates the fleet transmit power level and redistributes the
//!   existing plan under a new epoch.
//! - [`Command::RememberBle`]: Turns on or off remembering the node last given the Bluetooth scan,
//!   which the engine hands the scan back to whenever that node is assignable and none holds it.
//! - [`Command::SetPool`]: Changes the channel pool and re-cuts the whole fleet against it.
//!
//! # Logging
//!
//! `tracing` is the one side channel the engine writes to. It logs each time the host falls
//! behind the bridge and each time it catches up, with the pair of bridge stamps that decided
//! it ([`FleetEngine::note_arrival`]): those stamps are stored nowhere else, so a lag spike in a
//! capture is otherwise unexplainable.

mod admin;
mod config;
mod event;
mod lag;
mod node;
mod replan;
mod rx;
mod snapshot;

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::link::{BridgeToHost, HostToBridge};
use wartui_proto::mac::Mac;
use wartui_proto::plan::{Job, Plan, clamp_tx_power};

pub use config::EngineConfig;
pub use event::{ActionBatch, Command, Event, HostSample, Now, StorePeaks, StoreStats};
pub use node::{Assignment, NodeState};
pub use snapshot::{BridgeStatus, Counters, NodeView, Snapshot, TailEntry};

pub(crate) use lag::BEHIND_THE_AIR_US;
pub(crate) use rx::advance_since_boot;

use crate::distinct::Distinct;
use crate::record::{BridgeStatusSeen, HostStatus, Record};
use admin::Pending;
#[cfg(doc)]
use lag::QUIET_CONNECT;
#[cfg(doc)]
use replan::next_epoch;

/// The fleet's state and the rules that advance it.
#[derive(Debug)]
pub struct FleetEngine {
    config: EngineConfig,
    nodes: BTreeMap<Mac, NodeState>,
    bridge: Option<BridgeInfo>,
    link_up: bool,
    /// When the link last came up, or `None` while it is down. See
    /// [`Self::evict_stale_peers`].
    link_up_since: Option<Instant>,
    link_error: Option<String>,
    counters: Counters,
    tail: VecDeque<TailEntry>,
    unique_wifi: Distinct,
    unique_ble: Distinct,
    bridge_status: Option<BridgeStatus>,
    /// The bridge's drop count when this host attached, subtracted from every
    /// later reading. `None` until the first status of a connection arrives.
    dropped_baseline: Option<u32>,
    last_status_poll: Option<Instant>,
    started_at_ms: i64,
    /// Assignments and clears on the air, keyed by the id the bridge will echo
    /// back.
    pending: BTreeMap<u16, Pending>,
    /// Wraps, and harmlessly: an id only has to be unique among the handful of
    /// assignments outstanding at once, not for the life of the session.
    next_send_id: u16,
    /// The last epoch handed out. Starts at 0 with every capture and is not persisted:
    /// a node may still hold an epoch from an earlier run, and [`next_epoch`] never
    /// hands a node the epoch its heartbeat says it holds, so a repeat cannot land as a
    /// discarded duplicate.
    last_counter: u64,
    /// The partition in force.
    plan: Option<Plan>,
    /// The members that plan was built for, in the order that gave them their
    /// slot, each with the job it was cut a share for. Compared against
    /// the live membership to decide whether to re-partition — the job included,
    /// because a node whose token changed band, or which has taken the Bluetooth
    /// scan, has a share of the wrong shape while the membership is unchanged.
    plan_members: Vec<(Mac, Job)>,
    /// The one node asked to scan Bluetooth, if any.
    ///
    /// Held beside the plan rather than in it because it is an operator's choice —
    /// made now, or carried forward from [`Self::preferred_ble`] — and the plan is a
    /// function of who is present; it reaches the planner as a [`Job::Bluetooth`]
    /// slot on every re-cut, which is what takes that node's Wi-Fi share away and
    /// hands it round.
    ble_node: Option<Mac>,
    /// Whether [`Command::AssignBle`] records its target in [`Self::preferred_ble`].
    remember_ble: bool,
    /// The operator's last Bluetooth choice, carried forward: [`Self::replan`] gives
    /// this node the scan whenever it is assignable and no node holds it. Always
    /// `None` while [`Self::remember_ble`] is off.
    preferred_ble: Option<Mac>,
    /// The previous frame's bridge stamp and the host instant it was handled
    /// on, which together say whether this host is reading the link in real
    /// time or working through a backlog. `None` until the first frame since the
    /// engine started or the link last came up or went down. See
    /// [`FleetEngine::note_arrival`].
    last_arrival: Option<(u32, Instant)>,
    /// How far behind the air this host currently is, in microseconds. Zero
    /// whenever the link is read live; it climbs only while frames arrive faster
    /// than wall-clock time can account for. [`BEHIND_THE_AIR_US`] on start and on
    /// every connect or disconnect, until a frame proves otherwise: one the host
    /// waited for, or a first frame after [`QUIET_CONNECT`] of silence.
    backlog_lag_us: u64,
    /// The largest [`Self::backlog_lag_us`] measured since the last
    /// [`Event::HostSample`]. Taken only where a lag is computed from two arrivals,
    /// so the [`BEHIND_THE_AIR_US`] assumed on connecting is never reported on its
    /// own. The first lag measured after a connect builds on that assumption, so a
    /// backlog drained then reports at least it, which is what it was.
    lag_peak_us: u64,
    /// When the current measured spell behind the air began, or `None` while the
    /// host is live or behind only by assumption. Opens and closes the log lines
    /// [`FleetEngine::note_arrival`] writes.
    behind_since: Option<Instant>,
    /// The largest lag measured during the spell [`Self::behind_since`] opened.
    behind_peak_us: u64,
}

impl FleetEngine {
    /// Start an engine. `now` fixes the session's start time.
    #[must_use]
    pub fn new(mut config: EngineConfig, now: Now) -> Self {
        // Clamped once, here, rather than at either place a power reaches a radio:
        // `plan::clamp_tx_power` says why a refused one must not be reachable.
        config.tx_power = clamp_tx_power(config.tx_power);
        config.bridge_tx_power = clamp_tx_power(config.bridge_tx_power);
        Self {
            nodes: BTreeMap::new(),
            bridge: None,
            link_up: false,
            link_up_since: None,
            link_error: None,
            counters: Counters::default(),
            tail: VecDeque::new(),
            unique_wifi: Distinct::new(),
            unique_ble: Distinct::new(),
            bridge_status: None,
            dropped_baseline: None,
            last_status_poll: None,
            started_at_ms: now.unix_ms,
            pending: BTreeMap::new(),
            next_send_id: 1,
            last_counter: 0,
            plan: None,
            plan_members: Vec::new(),
            ble_node: None,
            remember_ble: config.remember_ble,
            preferred_ble: config.preferred_ble.filter(|_| config.remember_ble),
            last_arrival: None,
            // Pessimistic from the start, as on `Connected`: the port opens onto a
            // backlog, and its frames reach the engine before the bridge's `Ready`.
            backlog_lag_us: BEHIND_THE_AIR_US,
            lag_peak_us: 0,
            behind_since: None,
            behind_peak_us: 0,
            config,
        }
    }

    /// Advance the state machine. No I/O, no clock reads, no allocation beyond
    /// the batch itself.
    pub fn handle(&mut self, event: Event, now: Now) -> ActionBatch {
        let mut batch = ActionBatch::default();
        match event {
            Event::Tick => self.on_tick(now, &mut batch),
            Event::Command(command) => self.on_command(command, now, &mut batch),
            Event::HostSample(sample) => self.on_host_sample(sample, now, &mut batch),
            Event::Link(LinkEvent::Connected(info)) => {
                batch.records.push(Record::Bridge(crate::record::BridgeSeen {
                    mac: info.mac,
                    chip: format!("{:?}", info.chip),
                    fw_version: info.fw_version.clone(),
                }));
                // A node unicasts its sightings to whichever bridge last sent it an
                // admin or clear frame, never learning it from a heartbeat, which
                // still broadcasts. `self.bridge` is not cleared on `Disconnected`,
                // so if one was already held and its MAC differs, every node is
                // still aimed at a bridge that is gone, with heartbeats alone
                // keeping the fleet looking healthy. Re-sending each assignment
                // under a fresh epoch puts an admin frame in that node's next
                // window, which is what moves it onto the new bridge's address.
                if self.bridge.as_ref().is_some_and(|bridge| bridge.mac != info.mac) {
                    let macs: Vec<Mac> = self.nodes.keys().copied().collect();
                    for mac in macs {
                        self.reissue(mac);
                    }
                }
                self.bridge = Some(info);
                self.link_up = true;
                self.link_up_since = Some(now.mono);
                self.link_error = None;
                // Whatever the bridge has been holding arrives now, so this host
                // is behind the air until a frame turns up that it had to wait
                // for, or until the link has sat quiet for `QUIET_CONNECT` with
                // nothing arriving. Starting pessimistic costs at most one admin
                // window and keeps the first frame of a long backlog from being
                // the one stale window this cannot recognize.
                self.reset_arrivals();
                // Its peer table starts empty, whether this is a new bridge or
                // the same one rebooted, so a node it had no room for before
                // may fit now.
                for node in self.nodes.values_mut() {
                    node.peer_refused = false;
                }
                // A new connection means a new baseline, whether the
                // bridge itself rebooted.
                self.dropped_baseline = None;
                // Straight away rather than waiting out the interval: a bridge that
                // restarted has gone back to its firmware fallback power, and this host
                // has no status for it at all.
                self.poll_bridge(now, &mut batch);
            }
            Event::Link(LinkEvent::Disconnected { reason }) => {
                self.link_up = false;
                self.link_up_since = None;
                self.link_error = Some(reason);
                self.reset_arrivals();
            }
            Event::Link(LinkEvent::Garbled(_)) => self.counters.garbled += 1,
            Event::Link(LinkEvent::Message(msg)) => self.on_message(&msg, now, &mut batch),
        }
        batch
    }

    /// Record the host's state: the runtime's sample, this engine's counters, and the
    /// lag peak, which starts again from nothing for the next sample.
    fn on_host_sample(&mut self, sample: HostSample, now: Now, batch: &mut ActionBatch) {
        let c = &self.counters;
        batch.records.push(Record::HostStatus(HostStatus {
            at_ms: now.unix_ms,
            frames: c.frames,
            duplicate_batches: c.duplicate_batches,
            garbled: c.garbled,
            undecodable: c.undecodable,
            incompatible: c.incompatible,
            foreign_fleet: c.foreign_fleet,
            foreign_admin: c.foreign_admin,
            admin_windows_missed: c.admin_windows_missed,
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
        // A node aging out of topology is the passage of time rather than
        // anything arriving, so the tick is the only thing that can see it. A node
        // that has left the fleet is not scanning for us either way.
        //
        // `last_heartbeat` guards it: a node put in the table by a sighting alone
        // has no heartbeat behind it yet, and reading that as "gone" takes the
        // scan back before the node has had any chance to answer.
        let ble_gone = self.ble_node.is_some_and(|mac| {
            self.nodes
                .get(&mac)
                .is_some_and(|node| node.last_heartbeat.is_some() && !self.is_alive(node, now))
        });
        // Clearing this is only about what the snapshot claims; the withdrawal
        // itself rides in the re-cut below, which this runs before. A node that
        // has left the fleet is not a member, so the re-cut drops what it was
        // owed. If it comes back while it is the preferred node, `replan` hands
        // it the scan again; otherwise it comes back without it.
        let _ = self.ble_node.take_if(|_| ble_gone);
        self.evict_stale_peers(now, batch);
        self.replan(now);
        let due = self
            .last_status_poll
            .is_none_or(|last| now.mono.duration_since(last) >= self.config.status_interval);
        if due && self.link_up {
            self.poll_bridge(now, batch);
        }
    }

    /// Free the bridge's peer slot of every node past the topology timeout.
    ///
    /// Such a node is out of the plan and is sent nothing, so its slot costs it
    /// nothing. On `urgent` because `bulk` drops when full, and a lost removal
    /// leaves this host believing in a free slot; it also has to reach the bridge
    /// before the assignment a re-admitted node gets on its next heartbeat. A node
    /// that returns is re-peered by its next send's `ensure_peer`.
    ///
    /// Only once the link has been up for a whole timeout. An outage silences every
    /// node at once, and after a reconnect the bridge's backlog of held heartbeats
    /// may not have arrived yet, so before then a live node looks a minute silent.
    fn evict_stale_peers(&mut self, now: Now, batch: &mut ActionBatch) {
        let settled = self
            .link_up_since
            .is_some_and(|since| now.mono.duration_since(since) >= self.config.topology_timeout);
        if !settled {
            return;
        }
        let stale: Vec<Mac> = self
            .nodes
            .values()
            .filter(|node| node.peered && !self.is_alive(node, now))
            .map(|node| node.mac)
            .collect();
        if stale.is_empty() {
            return;
        }
        for mac in stale {
            batch.urgent.push(HostToBridge::RemovePeer { mac });
            if let Some(node) = self.nodes.get_mut(&mac) {
                node.peered = false;
            }
        }
        // A slot has freed, so the re-cut that follows takes refused nodes back.
        for node in self.nodes.values_mut() {
            node.peer_refused = false;
        }
    }

    /// Ask the bridge how it is, and tell it what to transmit at.
    ///
    /// Paired on purpose and repeated every `status_interval` rather than sent once on
    /// connection. `bulk` drops rather than blocks, and a dropped `SetTxPower` has
    /// nothing behind it to notice: the bridge would spend the rest of the session at
    /// its firmware fallback while this host's snapshot, the panel and the link-budget
    /// reasoning read the configured value. The poll beside it already recovers that
    /// way, which is the whole argument for carrying the power with it rather than
    /// making it urgent — nothing here is racing a node's admin window, and the urgent
    /// queue is small precisely so that nothing but an assignment waits in it.
    ///
    /// `esp_wifi_set_max_tx_power` is idempotent and the bridge logs only a power that
    /// changed, so the repeat costs one small frame per interval and no log line.
    fn poll_bridge(&mut self, now: Now, batch: &mut ActionBatch) {
        self.last_status_poll = Some(now.mono);
        batch.bulk.push(HostToBridge::SetTxPower { power: self.config.bridge_tx_power });
        batch.bulk.push(HostToBridge::GetStatus);
    }

    fn on_message(&mut self, msg: &BridgeToHost, now: Now, batch: &mut ActionBatch) {
        match msg {
            BridgeToHost::Rx { src, dst, rssi, rx_us, payload } => {
                // Before `on_rx`: the heartbeat it handles reads `air_is_live`.
                self.note_arrival(*src, *rx_us, now);
                self.on_rx(*src, *dst, *rssi, *rx_us, payload, now, batch);
            }
            BridgeToHost::SendResult { id, status, tx_us } => {
                self.on_send_result(*id, *status, *tx_us, now, batch);
            }
            BridgeToHost::Status { peer_count, rx_count, dropped_tx, uptime_ms } => {
                // A count below the baseline means the bridge restarted and
                // began again from zero, so the old baseline is meaningless.
                let baseline = match self.dropped_baseline {
                    Some(baseline) if baseline <= *dropped_tx => baseline,
                    _ => *dropped_tx,
                };
                self.dropped_baseline = Some(baseline);
                self.bridge_status = Some(BridgeStatus {
                    peer_count: *peer_count,
                    rx_count: *rx_count,
                    dropped_tx: *dropped_tx,
                    dropped_since_attach: dropped_tx - baseline,
                    uptime_ms: *uptime_ms,
                });
                batch.records.push(Record::BridgeStatus(BridgeStatusSeen {
                    rx_at_ms: now.unix_ms,
                    peer_count: *peer_count,
                    rx_count: *rx_count,
                    dropped_tx: *dropped_tx,
                    uptime_ms: *uptime_ms,
                    host_frames: self.counters.frames,
                }));
            }
            // `Ready` reaches the engine as `LinkEvent::Connected`; the bridge's
            // own diagnostics are the operator's business, not the engine's.
            BridgeToHost::Ready { .. } | BridgeToHost::Log { .. } | BridgeToHost::Error { .. } => {}
        }
    }

    /// Whether a node has heartbeated recently enough to hold an assignment.
    #[must_use]
    pub fn is_alive(&self, node: &NodeState, now: Now) -> bool {
        node.last_heartbeat
            .is_some_and(|last| now.mono.duration_since(last) < self.config.topology_timeout)
    }

    /// Whether a node can be given channels at all.
    ///
    /// Alive is necessary and not sufficient. Two kinds of node heartbeat
    /// perfectly well and still never scan what they are sent: one the bridge has
    /// no peer slot for, and one this host has not heard declare which band its
    /// radio reaches. A share cut for either is a share nobody scans, which is
    /// worse than one node fewer — the fleet then covers less of the pool than it
    /// would have without that node present at all. Every other site that needs
    /// this rule points here.
    #[must_use]
    pub fn is_assignable(&self, node: &NodeState, now: Now) -> bool {
        self.is_alive(node, now) && node.capabilities.is_some() && !node.peer_refused
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
