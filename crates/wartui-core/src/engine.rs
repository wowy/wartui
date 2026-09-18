//! The fleet engine: a pure synchronous state machine.
//!
//! [`FleetEngine::handle`] reads no clock, touches no socket and opens no file.
//! Every effect leaves as an [`ActionBatch`] for someone else to perform, and
//! every input arrives as an [`Event`] with the time already decided by the
//! caller — so it cannot block the link by accident, and the whole of the
//! fleet's behaviour is testable against a clock a test invents.
//!
//! Transmitting lives here as well: allocating an epoch, waiting for the
//! heartbeat that opens a node's 100 ms admin window, and believing the
//! assignment landed only on a MAC-layer acknowledgement. The engine holds a
//! partition of the pool across every heartbeating node and re-cuts it when that
//! set changes, and that partition is the only thing an assignment ever carries:
//! nothing outside [`FleetEngine::replan`] decides what a node scans.
//!
//! [`Command::AssignBle`] is the exception, and only about which node scans
//! Bluetooth rather than what it scans. It lives here rather than in the view
//! because it is the same kind of fact as a channel assignment: something one
//! node holds, delivered inside that node's own admin window, believed only on
//! an acknowledgement.
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::air::{
    AdminMsg, Capabilities, DecodeError, Frame, RecordKind, foreign, wire_epoch,
};
use wartui_proto::link::{BridgeToHost, EspNowPayload, HostToBridge, Mac, SendStatus};
use wartui_proto::plan::{self, ChannelPool, ChannelSet, Plan, Radio};

use crate::distinct::Distinct;
use crate::position::PositionChain;
use crate::record::{
    AdminOutcome, AssignmentSent, Heartbeat, NodeSeen, Observation, RawFrame, Record, ssid_text,
};

/// The time, in both of the forms this code needs.
///
/// Durations are measured on the monotonic clock, which cannot jump backwards
/// over an NTP correction and declare the fleet dead; stored timestamps use the
/// wall clock, because a capture has to line up against something else. Both
/// travel together so a caller cannot reach for whichever is nearest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// Monotonic, for elapsed-time decisions.
    pub mono: Instant,
    /// Unix milliseconds, for anything written down.
    pub unix_ms: i64,
}

/// Something the engine must react to.
// `Link` dwarfs `Tick` because it carries an inline ESP-NOW payload. Boxing it
// would mean an allocation for every frame received in order to shrink a value
// that is created, matched on and dropped inside one call to `handle`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    /// Traffic, or the lack of it, from the bridge.
    Link(LinkEvent),
    /// The periodic tick. Drives liveness ageing and the status poll.
    Tick,
    /// Something the operator asked for.
    Command(Command),
}

/// An operator's instruction to the fleet.
///
/// Deliberately an event like any other rather than a method on the engine: a
/// keypress and a heartbeat have to be ordered against each other, and routing
/// both through [`FleetEngine::handle`] is what makes that ordering testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Move the Bluetooth scan to one node, or take it away from the fleet.
    ///
    /// At most one node, and by default none: NimBLE holds the one 2.4 GHz
    /// antenna through exactly the window a node has to be listening in, which
    /// cost a stock node every assignment sent to it. Our firmware bounds the
    /// scan and pays a ~10% sweep-period cost instead
    /// (`docs/phase-0-findings.md`, `docs/phase-1-findings.md`), so it is a cost
    /// the operator chooses on one node rather than one the fleet pays.
    ///
    /// Nothing goes out now: the flag rides on that node's next assignment, and
    /// a node only listens in the 100 ms it holds open after a heartbeat. Moving
    /// it costs two frames, because the node giving it up has to be told as well.
    AssignBle {
        /// Which node, or `None` to stop scanning BLE anywhere.
        mac: Option<Mac>,
    },
}

/// What the engine wants done as a result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActionBatch {
    /// Rows for the store, in the order they should be written.
    pub records: Vec<Record>,
    /// Commands that can wait behind anything else.
    pub bulk: Vec<wartui_proto::link::HostToBridge>,
    /// Commands racing a node's 100 ms admin window, sent ahead of anything
    /// in `bulk`. In practice: assignments, and only ever in the moment after
    /// a heartbeat.
    pub urgent: Vec<wartui_proto::link::HostToBridge>,
}

impl ActionBatch {
    /// Whether there is anything at all to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.bulk.is_empty() && self.urgent.is_empty()
    }
}

/// How the engine should behave.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Which channels the fleet is meant to scan. Recorded in the session,
    /// shown in the UI, and the set the engine partitions across the fleet.
    pub pool: ChannelPool,
    /// How long a node may go without a heartbeat before it stops counting
    /// towards topology. Matches the firmware's own 60 s node timeout.
    pub topology_timeout: Duration,
    /// How often to ask the bridge for its counters.
    pub status_interval: Duration,
    /// How many recent observations the snapshot carries for the UI.
    pub tail_len: usize,
    /// Keep the undecoded bytes of every frame as well.
    pub record_raw: bool,
    /// Where the host believes it is.
    pub position: PositionChain,
    /// How long to wait for a bridge to report what became of an assignment
    /// before writing it down as unanswered. Generous: the bridge blocks on
    /// the transmit callback, and a busy radio can take tens of milliseconds.
    pub admin_timeout: Duration,
    /// The last assignment epoch any wartui is known to have used against this
    /// database, from [`crate::Store::assignment_base`]. Epochs are allocated
    /// from `base + 1` upwards.
    ///
    /// Persisted, so a restarted host never reissues an epoch a node already
    /// holds: the node would acknowledge it and then discard it.
    pub assignment_base: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            pool: ChannelPool::Us,
            topology_timeout: Duration::from_secs(60),
            status_interval: Duration::from_secs(5),
            tail_len: 200,
            record_raw: false,
            position: PositionChain::empty(),
            admin_timeout: Duration::from_secs(2),
            assignment_base: 0,
        }
    }
}

/// Everything the engine knows about one node.
#[derive(Debug, Clone)]
pub struct NodeState {
    /// Full six-byte MAC.
    pub mac: Mac,
    /// Unix milliseconds of the first frame in this session.
    pub first_seen_ms: i64,
    /// Unix milliseconds of the most recent frame of any kind.
    pub last_seen_ms: i64,
    /// Monotonic time of the most recent frame of any kind.
    ///
    /// Any frame refreshes this; only a heartbeat refreshes `last_heartbeat`.
    /// Two clocks, because they answer different questions.
    pub last_seen: Instant,
    /// Monotonic time of the most recent heartbeat, which is what decides
    /// whether this node can still be given a channel assignment.
    pub last_heartbeat: Option<Instant>,
    /// The node's most recent heartbeat counter.
    pub counter: Option<u32>,
    /// How many times the counter has gone backwards, meaning the node
    /// rebooted and has forgotten whatever assignment it held.
    pub reboots: u32,
    /// Heartbeats received in this session.
    pub heartbeats: u64,
    /// Observations received in this session.
    pub observations: u64,
    /// Most recent link RSSI, as the bridge measured it.
    pub link_rssi: Option<i8>,
    /// What the node said it is, from its most recent heartbeat.
    ///
    /// `None` means only that nothing but a sighting has been heard from this
    /// address yet — a node reports what it found on a channel before it gets
    /// back to the control channel. Such a node is left out of the plan; see
    /// [`FleetEngine::is_assignable`]. Taken from every heartbeat rather than
    /// remembered from the first, so a node reflashed with a different build
    /// stops claiming the old one's features.
    pub capabilities: Option<Capabilities>,
    /// The bridge's peer table had no room for this node.
    ///
    /// It cannot be transmitted to at all until a slot frees up, so it is no use
    /// to a plan; see [`FleetEngine::is_assignable`]. Cleared when a bridge
    /// announces itself, since its table starts empty.
    pub peer_refused: bool,
    /// What this host wants the node to be scanning.
    pub desired: Option<Assignment>,
    /// What the node acknowledged, which is a different thing. Cleared when it
    /// reboots, because a reboot means it has forgotten.
    pub confirmed: Option<Assignment>,
    /// Whether [`Self::desired`] still needs to be delivered. Set when the plan
    /// changes, when the Bluetooth scan moves and when the node reboots; cleared
    /// only on an acknowledgement, never on a successful enqueue.
    pub dirty: bool,
    /// How many times an assignment has been put on the air for this node.
    pub admin_attempts: u32,
    /// What happened to the most recent attempt.
    pub last_outcome: Option<AdminOutcome>,
    /// Heartbeat-to-transmit-callback microseconds of the most recent
    /// acknowledged assignment, as the bridge measured it.
    pub last_latency_us: Option<u32>,
    /// Bridge-local microsecond stamp of the most recent heartbeat, which is
    /// the near end of that measurement.
    last_heartbeat_rx_us: Option<u32>,
    /// Gaps between recent heartbeats, in milliseconds, newest last.
    beat_gaps: VecDeque<u32>,
}

/// What one node was told to do, and the fleet arithmetic it was computed
/// against.
///
/// The index and count travel with the channels rather than being read live at
/// send time — divergence 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// Which [`wartui_proto::plan::SCAN_CHANNELS`] indices to dwell on.
    pub channels: ChannelSet,
    /// Whether this node is the one scanning Bluetooth.
    ///
    /// Part of the assignment rather than beside it: it travels in the same frame
    /// and is adopted by the same epoch comparison, so treating them separately
    /// would make "acknowledged" ambiguous.
    pub ble: bool,
    /// This node's slot in the fleet-wide stagger order.
    pub node_index: u8,
    /// Fleet size as of this assignment.
    pub node_count: u8,
    /// The persisted monotonic epoch it was allocated from.
    pub counter: u64,
}

impl NodeState {
    /// A node that has just been heard from for the first time.
    #[must_use]
    pub fn new(mac: Mac, now: Now) -> Self {
        Self {
            mac,
            first_seen_ms: now.unix_ms,
            last_seen_ms: now.unix_ms,
            last_seen: now.mono,
            last_heartbeat: None,
            counter: None,
            reboots: 0,
            heartbeats: 0,
            observations: 0,
            link_rssi: None,
            capabilities: None,
            peer_refused: false,
            desired: None,
            confirmed: None,
            dirty: false,
            admin_attempts: 0,
            last_outcome: None,
            last_latency_us: None,
            last_heartbeat_rx_us: None,
            beat_gaps: VecDeque::new(),
        }
    }

    /// How long this node is taking between heartbeats, in milliseconds.
    ///
    /// The median of the last few gaps rather than the last one. A node
    /// heartbeats once per completed sweep, so this is proportional to how many
    /// channels it is scanning — which is the whole proof that an assignment
    /// landed, visible without serial access to the node. The median is what
    /// keeps one lost heartbeat, which doubles a single gap, from reading as a
    /// range twice the size.
    #[must_use]
    pub fn beat_period_ms(&self) -> Option<u32> {
        if self.beat_gaps.is_empty() {
            return None;
        }
        let mut gaps: Vec<u32> = self.beat_gaps.iter().copied().collect();
        gaps.sort_unstable();
        Some(gaps[gaps.len() / 2])
    }

    fn note_beat_gap(&mut self, now: Now) {
        if let Some(previous) = self.last_heartbeat {
            let gap = now.mono.duration_since(previous).as_millis();
            if self.beat_gaps.len() >= BEAT_WINDOW {
                self.beat_gaps.pop_front();
            }
            self.beat_gaps.push_back(u32::try_from(gap).unwrap_or(u32::MAX));
        }
    }
}

/// How many heartbeat gaps to keep per node.
///
/// Five: enough for the median to survive one lost heartbeat, few enough that
/// the figure follows a new assignment within three sweeps rather than averaging
/// the old range in for a minute. Public because it is also how many heartbeats
/// anything measuring a period across a change of range has to wait out.
pub const BEAT_WINDOW: usize = 5;

/// Running totals, all of them since the engine started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// ESP-NOW frames the bridge forwarded.
    pub frames: u64,
    /// Frames that parsed as observations.
    pub observations: u64,
    /// Frames that parsed as heartbeats.
    pub heartbeats: u64,
    /// Frames that were not ours and were not the vendor's either.
    pub undecodable: u64,
    /// Frames of ours from a build speaking a wire version this one does not.
    /// A half-flashed fleet looks like this and nothing else would say so.
    pub incompatible: u64,
    /// Vendor heartbeats and observations: another fleet is on this channel,
    /// transmitting where these nodes are listening.
    pub foreign_fleet: u64,
    /// Assignments seen on the air that this host did not send — another core
    /// is powered up and driving a fleet nearby.
    pub foreign_admin: u64,
    /// USB frames that failed their checksum.
    pub garbled: u64,
    /// Assignments held back because the heartbeat that would have carried them
    /// was replayed out of the bridge's backlog, naming a window that shut
    /// before this host was listening. Counted only where one was owed, so the
    /// figure is transmits deferred and not one per stale frame.
    pub admin_windows_missed: u64,
    /// Assignments this host has put on the air.
    pub admin_sent: u64,
    /// Assignments a node's radio acknowledged.
    pub admin_acked: u64,
    /// Assignments that went out and were not acknowledged, were refused by
    /// the bridge, or were never answered for.
    pub admin_failed: u64,
    /// Assignments refused because the bridge's peer table was full, which
    /// means the fleet is larger than the twenty nodes wartui supports.
    pub peer_table_full: u64,
    /// How many times the pool has been re-partitioned across the fleet.
    pub replans: u64,
}

/// Counters the store keeps, folded into the snapshot for display.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    /// Rows written.
    pub written: u64,
    /// Rows dropped because the store could not keep up. Losing an observation
    /// beats stalling the engine, but it must be visible when it happens.
    pub dropped: u64,
}

/// A node as of one snapshot.
///
/// `assignable` is carried rather than left for the UI to re-derive, because the
/// rule is not the obvious one: a node can be streaming observations with a
/// healthy RSSI and still be unable to accept an assignment, since only
/// heartbeats open its admin window.
#[derive(Debug, Clone)]
pub struct NodeView {
    /// Everything known about the node.
    pub state: NodeState,
    /// Whether it has heartbeated inside the topology timeout.
    pub assignable: bool,
}

/// One row of the UI's observation stream.
#[derive(Debug, Clone, PartialEq)]
pub struct TailEntry {
    /// Which node reported it.
    pub node_mac: Mac,
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// The observed BSSID.
    pub bssid: [u8; 6],
    /// SSID as [`ssid_text`] renders it — this one is for human eyes, and the
    /// store keeps the original bytes. Empty means hidden, which the view says
    /// so, and is why the padding has to be gone before it gets here.
    pub ssid: String,
    /// The `AuthMode` token.
    pub security: String,
    /// Channel, or 0 for BLE.
    pub channel: u16,
    /// Signal strength as the node measured it.
    pub rssi: i16,
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
}

/// What the UI renders. Rebuilt on a tick, never streamed row by row.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The bridge, once it has announced itself.
    pub bridge: Option<BridgeInfo>,
    /// Whether the link is currently up.
    pub link_up: bool,
    /// Why it went down, if it did.
    pub link_error: Option<String>,
    /// The configured channel pool.
    pub pool: ChannelPool,
    /// The partition in force, when there is one. `None` with nothing
    /// heartbeating, or with more nodes alive than wartui supports.
    pub plan: Option<Plan>,
    /// Which node is scanning Bluetooth, if any.
    pub ble_node: Option<Mac>,
    /// Every node, ordered by MAC so the table does not reshuffle itself.
    pub nodes: Vec<NodeView>,
    /// How many are heartbeating inside the topology timeout.
    ///
    /// Deliberately not the same number as [`Self::assignable`]: a fleet that has
    /// outgrown the bridge's twenty peer slots is entirely alive and entirely
    /// undrivable, and one count for both leaves that unsayable.
    pub alive: usize,
    /// How many of those can actually be given an assignment.
    ///
    /// The planner partitions over exactly this set.
    pub assignable: usize,
    /// The most recent observations, newest last.
    pub tail: Vec<TailEntry>,
    /// Distinct Wi-Fi BSSIDs seen this session, estimated (about 0.8% standard error;
    /// see [`crate::distinct`]).
    pub unique_wifi_aps: u64,
    /// Distinct BLE addresses seen this session, estimated the same way.
    pub unique_ble_aps: u64,
    /// Engine totals.
    pub counters: Counters,
    /// Store totals.
    pub store: StoreStats,
    /// The bridge's own last reported counters.
    pub bridge_status: Option<BridgeStatus>,
    /// Unix milliseconds the session began.
    pub started_at_ms: i64,
    /// Unix milliseconds this snapshot was taken.
    pub now_ms: i64,
    /// Where the host believes it is, resolved as of this snapshot.
    pub position: crate::position::Fix,
    /// What the GPS is doing, when one is configured. Separate from
    /// [`Self::position`] because "no fix" and "no receiver" look identical in
    /// a row and are completely different problems to the person watching.
    pub gps: Option<crate::gps::GpsView>,
}

/// The bridge's self-report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStatus {
    /// Channel the radio is parked on.
    pub channel: u8,
    /// Registered peers.
    pub peer_count: u8,
    /// Frames received since boot.
    pub rx_count: u32,
    /// Frames the bridge's outbound ring has dropped since it booted.
    ///
    /// Mostly historical: a bridge left powered with nothing attached drops
    /// everything it hears, so the figure is large and meaningless on connect.
    pub dropped_tx: u32,
    /// Frames dropped since this host attached, which is the number that means
    /// something: it counts data this capture lost.
    pub dropped_since_attach: u32,
    /// Bridge uptime.
    pub uptime_ms: u32,
}

/// The fleet's state and the rules that advance it.
#[derive(Debug)]
pub struct FleetEngine {
    config: EngineConfig,
    nodes: BTreeMap<Mac, NodeState>,
    bridge: Option<BridgeInfo>,
    link_up: bool,
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
    /// Assignments on the air, keyed by the id the bridge will echo back.
    pending: BTreeMap<u16, PendingAdmin>,
    /// Wraps, and harmlessly: an id only has to be unique among the handful of
    /// assignments outstanding at once, not for the life of the session.
    next_send_id: u16,
    /// The last epoch handed out. Starts at the store's persisted base.
    last_counter: u64,
    /// The partition in force.
    plan: Option<Plan>,
    /// The members that plan was built for, in the order that gave them their
    /// `node_index`, each with the radio it was cut a share for. Compared against
    /// the live membership to decide whether to re-partition — radio included,
    /// because a node whose token changed band has a share of the wrong shape
    /// while the membership is unchanged.
    plan_members: Vec<(Mac, Radio)>,
    /// The one node asked to scan Bluetooth, if any. Not part of the plan: the
    /// plan is a function of who is present, and this is an operator's choice
    /// that survives re-partitioning.
    ble_node: Option<Mac>,
    /// The previous frame's bridge stamp and the host instant it was handled
    /// on, which together say whether this host is reading the link in real
    /// time or working through a backlog. See [`FleetEngine::note_arrival`].
    last_arrival: Option<(u32, Instant)>,
    /// How far behind the air this host currently is, in microseconds. Zero
    /// whenever the link is read live; it climbs only while frames arrive faster
    /// than wall-clock time can account for.
    backlog_lag_us: u64,
}

/// A lag large enough that [`FleetEngine::air_is_live`] says no, used as the
/// starting assumption on a connection whose backlog has not been seen yet.
const BEHIND_THE_AIR: u64 = plan::ADMIN_WAIT_MS as u64 * 1_000;

/// One assignment in flight.
#[derive(Debug, Clone, Copy)]
struct PendingAdmin {
    mac: Mac,
    assignment: Assignment,
    sent_mono: Instant,
    sent_ms: i64,
    /// The bridge's own microsecond stamp on the heartbeat that opened this
    /// node's admin window, so the latency is measured entirely on the bridge's
    /// clock and never picks up the host's scheduling noise.
    heartbeat_rx_us: Option<u32>,
}

impl FleetEngine {
    /// Start an engine. `now` fixes the session's start time.
    #[must_use]
    pub fn new(config: EngineConfig, now: Now) -> Self {
        Self {
            nodes: BTreeMap::new(),
            bridge: None,
            link_up: false,
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
            last_counter: config.assignment_base,
            plan: None,
            plan_members: Vec::new(),
            ble_node: None,
            last_arrival: None,
            // Pessimistic from the start, as on `Connected`: the port opens onto a
            // backlog, and its frames reach the engine before the bridge's `Ready`.
            backlog_lag_us: BEHIND_THE_AIR,
            config,
        }
    }

    /// Advance the state machine. No I/O, no clock reads, no allocation beyond
    /// the batch itself.
    pub fn handle(&mut self, event: Event, now: Now) -> ActionBatch {
        let mut batch = ActionBatch::default();
        match event {
            Event::Tick => self.on_tick(now, &mut batch),
            Event::Command(command) => self.on_command(command),
            Event::Link(LinkEvent::Connected(info)) => {
                batch.records.push(Record::Bridge(crate::record::BridgeSeen {
                    mac: info.mac,
                    chip: format!("{:?}", info.chip),
                    fw_version: info.fw_version.clone(),
                }));
                self.bridge = Some(info);
                self.link_up = true;
                self.link_error = None;
                // Whatever the bridge has been holding arrives now, so this host
                // is behind the air until a frame turns up that it had to wait
                // for. Starting pessimistic costs at most one admin window and
                // keeps the first frame of a long backlog from being the one
                // stale window this cannot recognise.
                self.last_arrival = None;
                self.backlog_lag_us = BEHIND_THE_AIR;
                // Its peer table starts empty, whether this is a new bridge or
                // the same one rebooted, so a node it had no room for before
                // may fit now.
                for node in self.nodes.values_mut() {
                    node.peer_refused = false;
                }
                // A new connection means a new baseline, whether or not the
                // bridge itself rebooted.
                self.dropped_baseline = None;
                // Ask straight away rather than waiting out the interval.
                self.last_status_poll = Some(now.mono);
                batch.bulk.push(wartui_proto::link::HostToBridge::GetStatus);
            }
            Event::Link(LinkEvent::Disconnected { reason }) => {
                self.link_up = false;
                self.link_error = Some(reason);
                self.last_arrival = None;
                self.backlog_lag_us = BEHIND_THE_AIR;
            }
            Event::Link(LinkEvent::Garbled(_)) => self.counters.garbled += 1,
            Event::Link(LinkEvent::Message(msg)) => self.on_message(&msg, now, &mut batch),
        }
        batch
    }

    fn on_tick(&mut self, now: Now, batch: &mut ActionBatch) {
        self.expire_pending(now, batch);
        // A node ageing out of topology is the passage of time rather than
        // anything arriving, so the tick is the only thing that can see it.
        // Checked outside `replan`, because the Bluetooth assignment is not the
        // planner's: the plan is a function of who is present, and this is an
        // operator's choice that a re-cut has no opinion about. A node that has
        // left the fleet is not scanning for us either way. A node that announces
        // itself without the `ble` feature loses it on the same tick and for the
        // same reason — it would adopt the flag, acknowledge, and scan nothing.
        //
        // Both cases are about a node that has *spoken*, which is why
        // `last_heartbeat` guards the first: a node put in the table by a
        // sighting alone has no heartbeat behind it yet, and reading that as
        // "gone" takes the scan back before the node has had any chance to
        // answer. Only `!is_alive` needs the guard — capabilities arrive in
        // heartbeats, so `Some` capabilities implies one by construction.
        let ble_gone = self.ble_node.is_some_and(|mac| {
            self.nodes.get(&mac).is_some_and(|node| {
                (node.last_heartbeat.is_some() && !self.is_alive(node, now))
                    || node.capabilities.is_some_and(|capabilities| !capabilities.ble)
            })
        });
        if let Some(mac) = self.ble_node.take_if(|_| ble_gone) {
            // Forgetting who held the scan is not taking it off them: the flag
            // travels in the assignment frame, so a node still holding one goes
            // on scanning — and goes on being *shown* as a holder, since the
            // fleet table reads the flag off the assignment. Re-issuing under a
            // fresh epoch is the only way to withdraw it. For a node that has
            // merely gone quiet that frame waits on a heartbeat that may never
            // come, which is right: if it returns, it returns without the scan.
            //
            // Only if the flag is really out there — a node that lost the scan by
            // changing its own token has already had it taken off by the reboot
            // re-issue, and a second epoch would have it re-adopt what it holds.
            let holds = self.nodes.get(&mac).and_then(|node| node.desired.or(node.confirmed));
            if holds.is_some_and(|assignment| assignment.ble) {
                self.reissue(mac);
            }
        }
        self.replan(now);
        let due = self
            .last_status_poll
            .is_none_or(|last| now.mono.duration_since(last) >= self.config.status_interval);
        if due && self.link_up {
            self.last_status_poll = Some(now.mono);
            batch.bulk.push(wartui_proto::link::HostToBridge::GetStatus);
        }
    }

    fn on_message(&mut self, msg: &BridgeToHost, now: Now, batch: &mut ActionBatch) {
        match msg {
            BridgeToHost::Rx { src, dst, rssi, channel, rx_us, payload } => {
                self.note_arrival(*rx_us, now);
                self.on_rx(*src, *dst, *rssi, *channel, *rx_us, payload, now, batch);
            }
            BridgeToHost::SendResult { id, status, tx_us } => {
                self.on_send_result(*id, *status, *tx_us, now, batch);
            }
            BridgeToHost::Status { channel, peer_count, rx_count, dropped_tx, uptime_ms } => {
                // A count below the baseline means the bridge restarted and
                // began again from zero, so the old baseline is meaningless.
                let baseline = match self.dropped_baseline {
                    Some(baseline) if baseline <= *dropped_tx => baseline,
                    _ => *dropped_tx,
                };
                self.dropped_baseline = Some(baseline);
                self.bridge_status = Some(BridgeStatus {
                    channel: *channel,
                    peer_count: *peer_count,
                    rx_count: *rx_count,
                    dropped_tx: *dropped_tx,
                    dropped_since_attach: dropped_tx - baseline,
                    uptime_ms: *uptime_ms,
                });
            }
            // `Ready` reaches the engine as `LinkEvent::Connected`; the bridge's
            // own diagnostics are the operator's business, not the engine's.
            BridgeToHost::Ready { .. } | BridgeToHost::Log { .. } | BridgeToHost::Error { .. } => {}
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the fields of one Rx frame, destructured at the call site"
    )]
    fn on_rx(
        &mut self,
        src: Mac,
        dst: Mac,
        rssi: i8,
        channel: u8,
        rx_us: u32,
        payload: &[u8],
        now: Now,
        batch: &mut ActionBatch,
    ) {
        self.counters.frames += 1;

        if self.config.record_raw {
            batch.records.push(Record::Raw(RawFrame {
                rx_at_ms: now.unix_ms,
                src,
                dst,
                rssi: Some(rssi),
                channel: Some(channel),
                bytes: payload.to_vec(),
            }));
        }

        let frame = match Frame::decode(payload) {
            Ok(frame) => frame,
            // Not ours at all. A vendor fleet on this channel is the one
            // undecodable thing with an operational meaning — it is
            // transmitting where these nodes are listening, and on a stock
            // fleet every scan is an active one — so it is counted where an
            // operator will look for it rather than as line noise.
            Err(DecodeError::BadMagic) => {
                match foreign::classify(payload) {
                    Some(foreign::Foreign::Admin) => self.counters.foreign_admin += 1,
                    Some(foreign::Foreign::Node) => self.counters.foreign_fleet += 1,
                    None => self.counters.undecodable += 1,
                }
                return;
            }
            // Ours, from a build this one cannot read — a fleet half-way through
            // a reflash, and the version byte is what lets it be said.
            Err(DecodeError::BadVersion(_)) => {
                self.counters.incompatible += 1;
                return;
            }
            Err(_) => {
                self.counters.undecodable += 1;
                return;
            }
        };

        // Decode before admitting anyone to the fleet: a sender whose frames are
        // not ours is not a node, and would otherwise sit in the table forever
        // as `no heartbeat`.
        match frame {
            // Nothing to do about it, but an operator chasing a fleet that keeps
            // changing its mind needs to know.
            Frame::Admin(_) => self.counters.foreign_admin += 1,
            Frame::Heartbeat(heartbeat) => {
                self.see_node(src, now, rssi, Some(heartbeat.capabilities), batch);
                self.counters.heartbeats += 1;
                let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
                // A counter below the last one means the node
                // restarted and has forgotten whatever range it was assigned.
                let rebooted = node.counter.is_some_and(|previous| heartbeat.counter < previous);
                if rebooted {
                    node.reboots += 1;
                    // Its epoch field went back to a boot value too, so the
                    // belief goes and the assignment is re-issued under a fresh
                    // epoch rather than one the node might now match.
                    node.confirmed = None;
                }
                node.capabilities = Some(heartbeat.capabilities);
                node.note_beat_gap(now);
                node.counter = Some(heartbeat.counter);
                node.last_heartbeat = Some(now.mono);
                node.last_heartbeat_rx_us = Some(rx_us);
                node.heartbeats += 1;
                batch.records.push(Record::Heartbeat(Heartbeat {
                    node_mac: src,
                    rx_at_ms: now.unix_ms,
                    counter: heartbeat.counter,
                    link_rssi: Some(rssi),
                }));

                if rebooted && node.desired.is_some() {
                    self.reissue(src);
                }
                // A node's first heartbeat is the moment it joins the fleet, and
                // every other node's range depends on how many there are.
                // Re-partitioning here rather than on the next tick is what lets
                // it take its share inside the window it has just opened.
                self.replan(now);
                // The node holds its window open for 100 ms and its radio is gone
                // after that, so this is the only moment in the sweep worth
                // transmitting in — as long as the heartbeat is news.
                // `send_admin` checks that for itself.
                self.send_admin(src, now, batch);
            }
            Frame::Sighting(sighting) => {
                self.see_node(src, now, rssi, None, batch);
                self.counters.observations += 1;
                match sighting.kind {
                    RecordKind::Wifi => {
                        self.unique_wifi.insert(&sighting.bssid);
                    }
                    RecordKind::Ble => {
                        self.unique_ble.insert(&sighting.bssid);
                    }
                }
                if let Some(node) = self.nodes.get_mut(&src) {
                    node.observations += 1;
                }
                // The trailer's meaning is the kind's — the roaming consortium
                // body for Wi-Fi, the company identifier for BLE — and this is
                // where that split is made. A trailer a well-formed frame of
                // this wire version cannot carry, a BLE one that is not exactly
                // two bytes, is dropped rather than guessed at.
                let (rcoi, mfgr_id) = match sighting.kind {
                    RecordKind::Wifi => {
                        ((!sighting.ext.is_empty()).then(|| sighting.ext.to_vec()), None)
                    }
                    RecordKind::Ble => (
                        None,
                        (sighting.ext.len() == 2)
                            .then(|| u16::from_le_bytes([sighting.ext[0], sighting.ext[1]])),
                    ),
                };
                let observation = Observation {
                    node_mac: src,
                    rx_at_ms: now.unix_ms,
                    link_rssi: Some(rssi),
                    bssid: sighting.bssid,
                    ssid: sighting.ssid.to_vec(),
                    security: sighting.security.to_string(),
                    channel: u16::from(sighting.channel),
                    rssi: i16::from(sighting.rssi),
                    kind: sighting.kind,
                    rcoi,
                    mfgr_id,
                    fix: self.config.position.resolve(now.unix_ms),
                    raw_body: payload.to_vec(),
                };
                self.push_tail(&observation);
                batch.records.push(Record::Observation(observation));
            }
        }
    }

    /// Refresh what is known about the node at `src`, and file the row that
    /// says it was here.
    ///
    /// Split out because a heartbeat and a sighting both prove a node exists but
    /// only one says what it is: an observation's payload is a network, and
    /// filing that as an identity would name a node after something it heard.
    fn see_node(
        &mut self,
        src: Mac,
        now: Now,
        rssi: i8,
        capabilities: Option<Capabilities>,
        batch: &mut ActionBatch,
    ) {
        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        node.last_seen = now.mono;
        node.last_seen_ms = now.unix_ms;
        node.link_rssi = Some(rssi);
        batch.records.push(Record::Node(NodeSeen {
            mac: src,
            first_seen_ms: node.first_seen_ms,
            last_seen_ms: now.unix_ms,
            capabilities: capabilities.map(|caps| caps.to_string()),
        }));
    }

    /// Take an operator's instruction. Nothing goes out from here.
    fn on_command(&mut self, command: Command) {
        match command {
            Command::AssignBle { mac } => self.on_assign_ble(mac),
        }
    }

    /// Move the Bluetooth scan, or take it off the fleet entirely.
    ///
    /// Nothing is sent from here. The flag lives in the assignment frame, so
    /// telling a node about it means re-issuing what it already holds under a
    /// fresh epoch; a node with nothing to re-issue gets the flag with whatever
    /// assignment reaches it next.
    fn on_assign_ble(&mut self, target: Option<Mac>) {
        // A node built without the `ble` feature would adopt the flag,
        // acknowledge, and scan nothing. The view refuses the keypress first; the
        // check is here too because `ble_node` is the only record of who was
        // asked and must not name a node that cannot answer. A node this host has
        // not heard from is *not* refused — naming one before it appears is
        // legitimate, and the tick takes the scan back once its token says so.
        if let Some(mac) = target
            && self
                .nodes
                .get(&mac)
                .is_some_and(|node| node.capabilities.is_some_and(|capabilities| !capabilities.ble))
        {
            return;
        }
        if self.ble_node == target {
            return;
        }
        let previous = std::mem::replace(&mut self.ble_node, target);
        // Both ends of the move, the node giving it up first. Each frame waits on
        // its own node's next heartbeat, so a new holder that heartbeats first
        // holds the scan alongside the old one until that one's window comes
        // round: the overlap is bounded by a heartbeat rather than excluded, and
        // "at most one node scans Bluetooth" is about what the host asks for.
        for mac in previous.into_iter().chain(target) {
            self.reissue(mac);
        }
    }

    /// Re-mark a node's assignment for delivery under a new epoch.
    ///
    /// Refreshes the Bluetooth flag on the way past: this is the only path by
    /// which a node already holding the right channels hears about a change to
    /// it, and a reboot re-issue must not put back a scan the fleet has since
    /// moved. A reflash is exactly a reboot whose token has changed, and the
    /// capabilities are read off the heartbeat before this runs, so the
    /// withdrawal travels in the frame the reboot was going to send anyway.
    fn reissue(&mut self, mac: Mac) {
        self.last_counter += 1;
        let counter = self.last_counter;
        let ble = self.ble_node == Some(mac);
        if let Some(node) = self.nodes.get_mut(&mac)
            && let Some(desired) = node.desired.as_mut()
        {
            desired.counter = counter;
            desired.ble = ble && node.capabilities.is_none_or(|capabilities| capabilities.ble);
            node.dirty = true;
        }
    }

    /// Hold the fleet on a partition of the pool, re-cutting it when the set of
    /// nodes changes and at no other time.
    ///
    /// Membership is every node currently heartbeating; see
    /// [`Self::is_assignable`]. Nodes are ordered by MAC, which is the order that
    /// gives them their `node_index`, so the numbering is a function of who is
    /// present rather than of the order they turned up in.
    ///
    /// Cheap on the common path: an unchanged membership returns without touching
    /// anything, which is what keeps this off a heartbeat's critical path. It is
    /// called from every tick, so that has to stay true.
    fn replan(&mut self, now: Now) {
        let members: Vec<(Mac, Radio)> = self
            .nodes
            .values()
            // Taken rather than defaulted, so no node reaches the planner with
            // a band it did not claim.
            .filter_map(|node| {
                node.capabilities
                    .filter(|_| self.is_assignable(node, now))
                    .map(|capabilities| (node.mac, Radio::from(capabilities)))
            })
            .collect();

        if members == self.plan_members {
            return;
        }

        // What a departed node was owed was computed for a fleet that no longer
        // exists, so it is dropped rather than queued against a node that has
        // stopped opening windows. What it last acknowledged stays, being still
        // the best guess at what it is scanning.
        let departed = std::mem::replace(&mut self.plan_members, members.clone());
        for (mac, _) in departed.iter().filter(|(mac, _)| !members.iter().any(|(m, _)| m == mac)) {
            if let Some(node) = self.nodes.get_mut(mac) {
                node.desired = None;
                node.dirty = false;
            }
        }

        let count = u8::try_from(members.len()).unwrap_or(u8::MAX);
        let radios: Vec<Radio> = members.iter().map(|(_, radio)| *radio).collect();
        // Not `plan`: a share of 5 GHz cut for an ESP32-C6 is a share nobody
        // scans, which is the failure the capability token exists to prevent,
        // reached by a node that is genuinely one of ours.
        let Some(plan) = plan::plan_for(self.config.pool, &radios) else {
            // Nothing heartbeating, or more nodes than the radio's peer table
            // can hold. Either way there is no partition to be in, and the
            // fleet keeps whatever it already had rather than being told
            // something the bridge could not deliver anyway.
            self.plan = None;
            return;
        };
        self.counters.replans += 1;
        self.plan = Some(plan);

        // One epoch per node that actually needs telling. Held locally because
        // the decision needs the node in hand, and `self` is borrowed for it.
        let mut counter = self.last_counter;
        let ble_node = self.ble_node;
        for (index, (mac, _)) in members.iter().enumerate() {
            let index = u8::try_from(index).unwrap_or(u8::MAX);
            // Nothing for this node: more nodes than the pool has channels *this
            // fleet* can reach, which with the radios read out of the tokens
            // means as few as twelve nodes with no 5 GHz between them. There is
            // no frame meaning "scan nothing", so it keeps what it holds —
            // duplicating another share rather than leaving a gap — and the
            // footer's unreachable line says the fleet is short of the pool.
            let Some(channels) = plan.channels_for(index) else { continue };
            let Some(node) = self.nodes.get_mut(mac) else { continue };
            // Masked the same way [`Self::reissue`] masks it: a node reflashed
            // without the `ble` feature would adopt the flag, acknowledge, and
            // scan nothing. The tick is what takes the scan off the fleet, and a
            // re-cut landing in between must not hand it straight back.
            let ble = ble_node == Some(*mac)
                && node.capabilities.is_none_or(|capabilities| capabilities.ble);

            let wanted = |a: Assignment| {
                a.channels == channels
                    && a.ble == ble
                    && a.node_index == index
                    && a.node_count == count
            };
            // Already scanning exactly this, or already queued to. Re-issuing
            // either would burn an epoch to tell a node what it already knows.
            if node.dirty {
                if node.desired.is_some_and(wanted) {
                    continue;
                }
            } else if node.confirmed.is_some_and(wanted) {
                // Nothing goes out, but what the plan wants and what the node
                // holds are now the same and have to be recorded as such. A node
                // rejoining a plan it already satisfies would otherwise have
                // nothing wanted of it, so its reboot re-issue would have nothing
                // to re-issue — and a node that has forgotten its assignment
                // parks on the control channel and goes silently blind.
                node.desired = node.confirmed;
                continue;
            }

            counter += 1;
            node.desired =
                Some(Assignment { channels, ble, node_index: index, node_count: count, counter });
            node.dirty = true;
        }
        self.last_counter = counter;
    }

    /// Track whether this host is reading the link in real time.
    ///
    /// The bridge buffers what it hears while nothing is attached, so a fresh
    /// connection receives a ring's worth of the recent past as fast as USB will
    /// carry it — minutes of bridge time inside milliseconds of host time
    /// (`docs/phase-4-findings.md`).
    ///
    /// Nothing in a frame says how old it is, but the bridge stamps every one
    /// with its own clock and the two clocks tick at the same rate: read live the
    /// stamps advance in step with the host's, and while a backlog drains they
    /// run far ahead. The gap accumulates into [`Self::backlog_lag_us`] and
    /// resets the moment the host waits longer for a frame than the bridge spent
    /// producing one, which can only happen with nothing queued.
    ///
    /// Deliberately an estimate rather than a clock synchronisation. It has one
    /// job: to keep [`Self::send_admin`] from mistaking the past for the present.
    fn note_arrival(&mut self, rx_us: u32, now: Now) {
        if let Some((last_rx_us, last_mono)) = self.last_arrival {
            let bridge_delta = u64::from(rx_us.wrapping_sub(last_rx_us));
            let host_delta = now.mono.saturating_duration_since(last_mono).as_micros() as u64;
            if host_delta >= bridge_delta {
                // The host out-waited the air: nothing is queued behind this.
                self.backlog_lag_us = 0;
            } else {
                self.backlog_lag_us = self.backlog_lag_us.saturating_add(bridge_delta - host_delta);
            }
        }
        self.last_arrival = Some((rx_us, now.mono));
    }

    /// Whether a frame being handled now is recent enough to act on.
    ///
    /// Only assignments care. A stale heartbeat is still a heartbeat — the node
    /// was alive and its radio is what it said — but the 100 ms window it opened
    /// shut long ago, so transmitting into it reaches nothing.
    fn air_is_live(&self) -> bool {
        self.backlog_lag_us < BEHIND_THE_AIR
    }

    /// Put a dirty node's assignment on the air, if it has one.
    ///
    /// Only ever called straight off a heartbeat: that is the one moment the
    /// node's radio is on the control channel and listening.
    fn send_admin(&mut self, mac: Mac, now: Now, batch: &mut ActionBatch) {
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);

        // Read before the node is borrowed and acted on after: a window that owed
        // nothing cost nothing, and counting those would bury the ones that did.
        let live = self.air_is_live();

        let Some(node) = self.nodes.get_mut(&mac) else { return };
        if !node.dirty {
            return;
        }
        let Some(assignment) = node.desired else { return };
        if !live {
            self.counters.admin_windows_missed += 1;
            return;
        }
        node.admin_attempts += 1;
        let heartbeat_rx_us = node.last_heartbeat_rx_us;

        let msg = AdminMsg {
            epoch: wire_epoch(assignment.counter),
            node_index: assignment.node_index,
            node_count: assignment.node_count,
            flags: AdminMsg::flags_for(assignment.ble),
            channels: assignment.channels,
        };
        // Fifteen bytes into a 250-byte buffer, so this cannot fail.
        let payload = EspNowPayload::from_slice(&msg.encode()).unwrap_or_default();

        self.pending.insert(
            id,
            PendingAdmin {
                mac,
                assignment,
                sent_mono: now.mono,
                sent_ms: now.unix_ms,
                heartbeat_rx_us,
            },
        );
        self.counters.admin_sent += 1;
        batch.urgent.push(HostToBridge::SendEspNow {
            id,
            dst: mac,
            // Add if absent and never remove: removing a peer races the transmit
            // callback for the frame just sent.
            ensure_peer: true,
            payload,
        });
    }

    /// The bridge said what became of an assignment.
    fn on_send_result(
        &mut self,
        id: u16,
        status: SendStatus,
        tx_us: u32,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        // An id we do not know is one of ours from before a reconnect, or a reply
        // to something else. Either way there is no assignment to resolve.
        let Some(pending) = self.pending.remove(&id) else { return };

        let outcome = match status {
            SendStatus::AckOk => AdminOutcome::Acked,
            SendStatus::AckFail => AdminOutcome::Unacked,
            // Broadcast is never acknowledged, and an assignment is always
            // unicast, so this is the bridge telling us the address was wrong.
            SendStatus::Broadcast
            | SendStatus::NoPeer
            | SendStatus::PeerTableFull
            | SendStatus::Rejected => AdminOutcome::Refused,
        };

        // A full peer table is terminal, not a miss: twenty peers is the entire
        // supported fleet, so there is no later heartbeat at which this becomes
        // possible. Give up on what was wanted and let the view say why.
        if matches!(status, SendStatus::PeerTableFull) {
            self.counters.peer_table_full += 1;
            if let Some(node) = self.nodes.get_mut(&pending.mac) {
                node.dirty = false;
                node.desired = None;
                // And it leaves the plan, so the next re-cut spreads the pool
                // over the nodes that can actually be reached.
                node.peer_refused = true;
            }
        }

        // Both stamps are the bridge's own microsecond clock, which wraps about
        // every 71 minutes; a wrapping subtraction is correct across it.
        //
        // The column means one thing — how long the assignment took to land
        // inside the window a heartbeat opened — so a figure larger than the
        // window is not that measurement and is not written down as one. That
        // happens on a heartbeat replayed out of a backlog, where the subtraction
        // is honest arithmetic on two honest stamps and still says nothing about
        // how fast the bridge answered. `None` is the truthful answer, and the
        // outcome is recorded either way.
        let latency_us = pending
            .heartbeat_rx_us
            .map(|rx_us| tx_us.wrapping_sub(rx_us))
            .filter(|us| *us <= plan::ADMIN_WAIT_MS * 1_000);

        self.resolve(&pending, outcome, latency_us, now, batch);
    }

    /// Give up on assignments the bridge never answered for.
    fn expire_pending(&mut self, now: Now, batch: &mut ActionBatch) {
        let timeout = self.config.admin_timeout;
        let stale: Vec<u16> = self
            .pending
            .iter()
            .filter(|(_, p)| now.mono.duration_since(p.sent_mono) >= timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(pending) = self.pending.remove(&id) {
                self.resolve(&pending, AdminOutcome::Silent, None, now, batch);
            }
        }
    }

    /// Record one assignment attempt's outcome, and update what we believe the
    /// node holds.
    fn resolve(
        &mut self,
        pending: &PendingAdmin,
        outcome: AdminOutcome,
        latency_us: Option<u32>,
        now: Now,
        batch: &mut ActionBatch,
    ) {
        let acked = outcome == AdminOutcome::Acked;
        if acked {
            self.counters.admin_acked += 1;
        } else {
            self.counters.admin_failed += 1;
        }

        if let Some(node) = self.nodes.get_mut(&pending.mac) {
            // Attempts resolve in the order they complete, not the order they
            // went out, so a failure from an attempt the node has moved past says
            // nothing about where it is now. It still gets its row — that attempt
            // did fail — but does not overwrite what landed afterwards.
            let superseded =
                node.confirmed.is_some_and(|c| c.counter >= pending.assignment.counter);
            if acked {
                node.last_outcome = Some(outcome);
                node.last_latency_us = latency_us;
                // Cleared on the MAC-layer acknowledgement, not on a successful
                // enqueue.
                //
                // Only if the node still wants what was sent: an operator who
                // changed their mind while this was in flight has already
                // marked it dirty again with a newer epoch.
                if node.desired.is_some_and(|d| d.counter == pending.assignment.counter) {
                    node.confirmed = Some(pending.assignment);
                    node.dirty = false;
                }
            } else if !superseded {
                node.last_outcome = Some(outcome);
            }
        }

        batch.records.push(Record::Assignment(AssignmentSent {
            node_mac: pending.mac,
            counter: pending.assignment.counter,
            wire_version: wire_epoch(pending.assignment.counter),
            node_index: pending.assignment.node_index,
            node_count: pending.assignment.node_count,
            channels: pending.assignment.channels,
            ble: pending.assignment.ble,
            created_at_ms: pending.sent_ms,
            // `Silent` means nothing came back at all, so there is no delivery
            // to stamp: a time here would be the timeout wearing an answer's look.
            delivered_at_ms: (outcome != AdminOutcome::Silent).then_some(now.unix_ms),
            outcome,
            latency_us,
        }));
    }

    fn push_tail(&mut self, observation: &Observation) {
        if self.config.tail_len == 0 {
            return;
        }
        while self.tail.len() >= self.config.tail_len {
            self.tail.pop_front();
        }
        self.tail.push_back(TailEntry {
            node_mac: observation.node_mac,
            rx_at_ms: observation.rx_at_ms,
            bssid: observation.bssid,
            ssid: ssid_text(&observation.ssid),
            security: observation.security.clone(),
            channel: observation.channel,
            rssi: observation.rssi,
            kind: observation.kind,
        });
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

    /// Build the view the UI renders.
    ///
    /// Taken on a tick rather than per event, so an observation burst does not
    /// become a burst of redraws.
    #[must_use]
    pub fn snapshot(&self, now: Now, store: StoreStats) -> Snapshot {
        let nodes: Vec<NodeView> = self
            .nodes
            .values()
            .map(|state| NodeView {
                assignable: self.is_assignable(state, now),
                state: state.clone(),
            })
            .collect();
        let alive = nodes.iter().filter(|n| self.is_alive(&n.state, now)).count();
        let assignable = nodes.iter().filter(|n| n.assignable).count();
        Snapshot {
            bridge: self.bridge.clone(),
            link_up: self.link_up,
            link_error: self.link_error.clone(),
            pool: self.config.pool,
            plan: self.plan,
            ble_node: self.ble_node,
            nodes,
            alive,
            assignable,
            tail: self.tail.iter().cloned().collect(),
            unique_wifi_aps: self.unique_wifi.estimate(),
            unique_ble_aps: self.unique_ble.estimate(),
            counters: self.counters,
            store,
            bridge_status: self.bridge_status,
            started_at_ms: self.started_at_ms,
            now_ms: now.unix_ms,
            position: self.config.position.resolve(now.unix_ms),
            gps: self.config.position.gps().map(crate::gps::Gps::view),
        }
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
