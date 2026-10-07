//! The fleet engine: a pure synchronous state machine.
//!
//! [`FleetEngine::handle`] reads no clock, touches no socket and opens no file. Inputs arrive as
//! [`Event`]s stamped by the caller, and effects return as an [`ActionBatch`]. So the engine cannot
//! block on the link, and tests run it against a simulated clock.
//!
//! Sending a node anything means allocating an epoch, waiting for the heartbeat that opens its
//! 100 ms admin window, and counting the frame delivered only on its MAC-layer ack. The engine
//! partitions the channel pool across live, heartbeating nodes, re-cutting when membership or the
//! pool changes. Only the planner's re-cut decides what a node scans.
//!
//! [`Command::AssignBle`] gives one node the Bluetooth scan as its whole job.
//! [`Command::RememberBle`] hands the scan back to the last node given it whenever that node is
//! assignable and none holds it. [`Command::SetTxPower`] re-sends the plan in force under fresh
//! epochs. [`Command::ClearRing`] empties dedup rings. [`Command::SetPool`] re-cuts the fleet
//! against a new pool.
//!
//! `tracing` is the engine's one side channel. It logs each fall behind the bridge and each
//! catch-up, with the pair of bridge stamps that decided it ([`FleetEngine::note_arrival`]). Those
//! stamps are stored nowhere else, so without the log a lag spike cannot be explained.

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
use wartui_proto::plan::{Job, Plan};
use wartui_proto::tx_power::clamp_tx_power;

pub use config::EngineConfig;
pub use event::{ActionBatch, Command, Event, HostSample, Now, StorePeaks, StoreStats};
pub use node::{Assignment, NodeState};
pub use snapshot::{BridgeStatus, Counters, NodeView, Snapshot, TailEntry};

pub(crate) use lag::BEHIND_THE_AIR_US;
pub(crate) use rx::advance_since_boot;

use crate::distinct::Distinct;
use crate::record::{BridgeStatusSeen, CaptureSettings, HostStatus, Record};
use admin::Pending;
#[cfg(doc)]
use lag::QUIET_CONNECT;
#[cfg(doc)]
use replan::next_epoch;
use rx::Rx;

/// The fleet's state and the rules that advance it.
#[derive(Debug)]
pub struct FleetEngine {
    config: EngineConfig,
    nodes: BTreeMap<Mac, NodeState>,
    bridge: Option<BridgeInfo>,
    link_up: bool,
    /// When the link last came up, `None` while down ([`Self::evict_stale_peers`]).
    link_up_since: Option<Instant>,
    link_error: Option<String>,
    counters: Counters,
    tail: VecDeque<TailEntry>,
    unique_wifi: Distinct,
    unique_ble: Distinct,
    bridge_status: Option<BridgeStatus>,
    /// The bridge's drop count when this host attached, subtracted from later readings.
    dropped_baseline: Option<u32>,
    last_status_poll: Option<Instant>,
    started_at_ms: i64,
    /// Assignments and clears on the air, keyed by the id the bridge will echo back.
    pending: BTreeMap<u16, Pending>,
    /// Wraps harmlessly: an id need only be unique among the few sends in flight.
    next_send_id: u16,
    /// The last epoch handed out. Starts at 0 each capture and is not persisted. A node may hold an
    /// epoch from an earlier run, so [`next_epoch`] skips the one its heartbeat reports.
    last_counter: u64,
    /// The partition in force.
    plan: Option<Plan>,
    /// The members the plan was cut for, in slot order, each with its job. [`Self::replan`]
    /// re-partitions when the live membership differs, job included: a node that changed band or
    /// took the Bluetooth scan holds a share of the wrong shape.
    plan_members: Vec<(Mac, Job)>,
    /// The one node asked to scan Bluetooth, if any.
    ///
    /// Beside the plan, not in it: it is an operator's choice (now, or carried from
    /// [`Self::preferred_ble`]), while the plan follows who is present. It reaches the planner as a
    /// [`Job::Bluetooth`] slot on every re-cut, which deals its Wi-Fi share to the rest.
    ble_node: Option<Mac>,
    /// Whether [`Command::AssignBle`] records its target in [`Self::preferred_ble`].
    remember_ble: bool,
    /// The operator's last Bluetooth choice. [`Self::replan`] gives this node the scan whenever it
    /// is assignable and none holds it. `None` while [`Self::remember_ble`] is off.
    preferred_ble: Option<Mac>,
    /// The previous frame's bridge stamp and the host instant it was handled, to tell live reading
    /// from a backlog ([`Self::note_arrival`]). `None` until the first frame since start or the
    /// last connect or disconnect.
    last_arrival: Option<(u32, Instant)>,
    /// How far behind the air this host is, in microseconds. Zero while the link is read live,
    /// climbing while frames arrive faster than wall-clock time allows. [`BEHIND_THE_AIR_US`] on
    /// start and every connect or disconnect, until a frame the host waited for, or a first frame
    /// after [`QUIET_CONNECT`] of silence.
    backlog_lag_us: u64,
    /// The largest [`Self::backlog_lag_us`] measured since the last [`Event::HostSample`]. Only
    /// lags computed from two arrivals count, so the assumed [`BEHIND_THE_AIR_US`] is never
    /// reported alone. A backlog drained after a connect builds on that assumption, so it reports
    /// at least that much, which is what it was.
    lag_peak_us: u64,
    /// When the current measured spell behind the air began. `None` while live, or behind only by
    /// assumption. Bounds the log lines of [`Self::note_arrival`].
    behind_since: Option<Instant>,
    /// The largest lag measured during the spell [`Self::behind_since`] opened.
    behind_peak_us: u64,
}

impl FleetEngine {
    /// Applied host settings for capture provenance. Radio adoption is recorded independently.
    #[must_use]
    pub fn capture_settings(&self) -> CaptureSettings {
        CaptureSettings {
            pool: self.config.pool,
            tx_power: self.config.tx_power,
            bridge_tx_power: self.config.bridge_tx_power,
            record_raw: self.config.record_raw,
            gps: self.config.position.gps().is_some(),
            gps_max_age: self.config.position.max_age(),
            remember_ble: self.remember_ble,
            preferred_ble: self.preferred_ble,
            ble_node: self.ble_node,
            topology_timeout: self.config.topology_timeout,
            status_interval: self.config.status_interval,
            admin_timeout: self.config.admin_timeout,
        }
    }
    /// Start an engine. `now` fixes the session's start time.
    #[must_use]
    pub fn new(mut config: EngineConfig, now: Now) -> Self {
        // Clamped once here, not where each power reaches a radio. `tx_power::clamp_tx_power` says
        // why.
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
            // Pessimistic, as on `Connected`: the port opens onto a backlog that arrives before
            // `Ready`.
            backlog_lag_us: BEHIND_THE_AIR_US,
            lag_peak_us: 0,
            behind_since: None,
            behind_peak_us: 0,
            config,
        }
    }

    /// Advance the state machine. No I/O and no clock reads.
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
                // A node sends its sightings to whichever bridge last sent it an admin or clear
                // frame. Heartbeats are broadcast and never tell it. `self.bridge` survives
                // `Disconnected`, so a new MAC here means every node is aimed at a gone bridge
                // while heartbeats look healthy. A re-issue puts an admin frame in each node's next
                // window, which moves it over.
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
                // The bridge's backlog arrives now. Count as behind until a frame the host waited
                // for, or `QUIET_CONNECT` of silence. Pessimism costs at most one admin window, and
                // keeps a long backlog's first frame from being the one stale window this cannot
                // recognize.
                self.reset_arrivals();
                // Its peer table starts empty, new bridge or rebooted, so refused nodes may fit
                // now.
                self.forget_peer_refusals();
                // A new connection means a new baseline, whether or not the bridge rebooted.
                self.dropped_baseline = None;
                // Poll now: a restarted bridge is back at its firmware fallback power, and there is
                // no status yet.
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
        // Only the tick sees a node age out of topology, and a gone node is not scanning for us.
        // `last_heartbeat` guards a node known only from a sighting: it has had no chance to answer
        // yet.
        let ble_gone = self.ble_node.is_some_and(|mac| {
            self.nodes
                .get(&mac)
                .is_some_and(|node| node.last_heartbeat.is_some() && !self.is_alive(node, now))
        });
        // This only changes what the snapshot claims. The re-cut below withdraws the scan, since a
        // gone node is not a member. If it returns as the preferred node, `replan` hands the scan
        // back.
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
    /// Such a node is out of the plan, so the slot costs it nothing. Removals go on `urgent`:
    /// `bulk` can drop them, leaving this host believing in a free slot, and they must beat the
    /// assignment a re-admitted node gets on its next heartbeat. A returning node is re-peered by
    /// its next send's `ensure_peer`.
    ///
    /// Waits until the link has been up a whole timeout. An outage silences every node at once, and
    /// until a reconnect's backlog of held heartbeats arrives, a live node looks a minute silent.
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
        self.forget_peer_refusals();
    }

    /// Let the next re-cut try every node the bridge's peer table refused.
    fn forget_peer_refusals(&mut self) {
        for node in self.nodes.values_mut() {
            node.peer_refused = false;
        }
    }

    /// Ask the bridge how it is, and tell it what to transmit at.
    ///
    /// The power rides every poll, not just the first. `bulk` drops rather than blocks, and nothing
    /// notices a dropped `SetTxPower`: the bridge would sit at its firmware fallback while the
    /// snapshot, panel and link-budget reasoning read the configured value. Repeating with the poll
    /// recovers that. It stays off `urgent`, which is kept small for what races an admin window. A
    /// mid-run change also goes out at once ([`Self::update_bridge_tx_power`]).
    /// `esp_wifi_set_max_tx_power` is idempotent and the bridge logs only changes, so the repeat
    /// costs one small frame per interval.
    fn poll_bridge(&mut self, now: Now, batch: &mut ActionBatch) {
        self.last_status_poll = Some(now.mono);
        batch.bulk.push(HostToBridge::SetTxPower { power: self.config.bridge_tx_power });
        batch.bulk.push(HostToBridge::GetStatus);
    }

    fn on_message(&mut self, msg: &BridgeToHost, now: Now, batch: &mut ActionBatch) {
        match msg {
            BridgeToHost::Rx { src, dst, rssi, rx_us, payload } => {
                let rx = Rx { src: *src, dst: *dst, rssi: *rssi, rx_us: *rx_us, payload };
                // Before `on_rx`: heartbeats read `air_is_live`.
                self.note_arrival(rx.src, rx.rx_us, now);
                self.on_rx(&rx, now, batch);
            }
            BridgeToHost::SendResult { id, status, tx_us } => {
                self.on_send_result(*id, *status, *tx_us, now, batch);
            }
            BridgeToHost::Status { peer_count, rx_count, dropped_tx, uptime_ms } => {
                // Below the baseline means the bridge restarted from zero.
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
            // `Ready` arrives as `LinkEvent::Connected`. Bridge diagnostics are the operator's, not
            // the engine's.
            BridgeToHost::Ready { .. } | BridgeToHost::Log { .. } | BridgeToHost::Error { .. } => {}
        }
    }

    /// Whether a node has heartbeated recently enough to hold an assignment.
    #[must_use]
    pub fn is_alive(&self, node: &NodeState, now: Now) -> bool {
        node.last_heartbeat
            .is_some_and(|last| now.mono.duration_since(last) < self.config.topology_timeout)
    }

    /// Whether a node can be given channels.
    ///
    /// Alive is not enough. A node with no peer slot, or that has not declared which band its radio
    /// reaches, heartbeats fine but never scans its share. A share nobody scans is worse than one
    /// node fewer: the fleet covers less of the pool than without it.
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
