//! The fleet engine: a pure synchronous state machine.
//!
//! [`FleetEngine::handle`] reads no clock, touches no socket and opens no file.
//! Every effect it wants leaves as an [`ActionBatch`] for someone else to
//! perform, and every input arrives as an [`Event`] with the time already
//! decided by the caller. That is what makes the whole of the fleet's
//! behaviour — liveness, reboot detection, what gets written down — testable
//! in microseconds against a clock a test invents, and what makes it
//! impossible for this code to block the link by accident.
//!
//! From Phase 4 it transmits, and the whole of that lives here as well:
//! allocating an epoch, waiting for the heartbeat that opens a node's 300 ms
//! admin window, putting the assignment down [`ActionBatch::urgent`], and
//! believing it landed only when the bridge reports a MAC-layer acknowledgement
//! — never when it reports a successful enqueue.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::air::{AdminMsg, Frame, MsgType, RecordKind, WardriveLine, wire_version};
use wartui_proto::link::{BridgeToHost, EspNowPayload, HostToBridge, Mac, SendStatus};
use wartui_proto::plan::{ChannelPool, IndexRun};

use crate::position::PositionChain;
use crate::record::{
    AdminOutcome, AssignmentSent, Heartbeat, NodeSeen, Observation, RawFrame, Record,
};

/// The time, in both of the forms this code needs.
///
/// Durations are measured on the monotonic clock, because that is the only one
/// that cannot jump backwards over an NTP correction and declare the whole
/// fleet dead. Stored timestamps use the wall clock, because a capture is
/// worthless if it cannot be lined up against anything else. Carrying both
/// together is what keeps a caller from reaching for whichever is nearest.
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
    /// Give one node a contiguous range of scan-channel indices.
    ///
    /// Nothing goes out immediately. A node only listens for an assignment in
    /// the 300 ms it holds open after a heartbeat (`src/WiFiOps.cpp:718-739`);
    /// the rest of the time its radio is away scanning some other channel. So
    /// this marks the node dirty and the frame goes on the next heartbeat.
    Assign {
        /// Which node.
        mac: Mac,
        /// The range it should scan.
        range: IndexRun,
    },
}

/// What the engine wants done as a result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActionBatch {
    /// Rows for the store, in the order they should be written.
    pub records: Vec<Record>,
    /// Commands that can wait behind anything else.
    pub bulk: Vec<wartui_proto::link::HostToBridge>,
    /// Commands racing a node's 300 ms admin window, sent ahead of anything
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
    /// Which channels the fleet is meant to scan. Recorded in the session and
    /// shown in the UI; not yet enforced, because enforcing it means
    /// transmitting.
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
    /// Divergence 4: the vendor core keeps this counter in RAM and resets it
    /// to 1 every boot (`src/WiFiOps.h:218`), so a restarted core that
    /// recomputes an assignment a node already holds is silently ignored.
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
    /// Divergence 2: the vendor firmware refreshes liveness only on a
    /// heartbeat — its `touchNode` call on the text path is commented out
    /// (`src/WiFiOps.cpp:1073-1082`) — so a node streaming observations whose
    /// heartbeats are being lost ages out at 60 s and churns the whole fleet's
    /// topology. Two clocks, because they answer different questions.
    pub last_seen: Instant,
    /// Monotonic time of the most recent heartbeat, which is what decides
    /// whether this node can still be given a channel range.
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
    /// The node sent a core-protocol frame, which only an encrypted node does.
    /// wartui cannot talk to it until encryption is turned off in its web UI.
    pub encrypted: bool,
    /// What this host wants the node to be scanning.
    pub desired: Option<Assignment>,
    /// What the node acknowledged, which is a different thing. Cleared when it
    /// reboots, because a reboot means it has forgotten.
    pub confirmed: Option<Assignment>,
    /// Whether [`Self::desired`] still needs to be delivered. Set when the
    /// operator asks and when the node reboots; cleared only on an
    /// acknowledgement, never on a successful enqueue.
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

/// A channel range, and the fleet arithmetic it was computed against.
///
/// The index and count travel with the range rather than being read live at
/// send time. Divergence 7: the vendor core reads `node_count` at the moment it
/// transmits (`src/WiFiOps.cpp:651`) while the ranges came from an earlier
/// recalculation, so a node that joins in between is told a fleet size that
/// disagrees with the partition its own range was cut from — and computes the
/// wrong transmit stagger slot from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// The inclusive run of [`wartui_proto::plan::SCAN_CHANNELS`] indices.
    pub range: IndexRun,
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
            encrypted: false,
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
/// the figure follows a new assignment within three sweeps rather than
/// averaging the old range in for a minute.
const BEAT_WINDOW: usize = 5;

/// Running totals, all of them since the engine started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// ESP-NOW frames the bridge forwarded.
    pub frames: u64,
    /// Frames that parsed as observations.
    pub observations: u64,
    /// Frames that parsed as heartbeats.
    pub heartbeats: u64,
    /// Frames whose ENOW header would not decode at all.
    pub undecodable: u64,
    /// Text frames whose body was not a valid wardrive line. A node emitting
    /// these is broken in a way silence would not distinguish.
    pub unparsed: u64,
    /// Core-protocol frames, which only an encrypted node sends.
    pub core_frames: u64,
    /// Assignments seen on the air that this host did not send — a vendor core
    /// is powered up and fighting us for the fleet.
    pub foreign_admin: u64,
    /// USB frames that failed their checksum.
    pub garbled: u64,
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
/// `assignable` is carried rather than left for the UI to re-derive, because
/// the rule is not the obvious one: a node can be plainly present — streaming
/// observations, RSSI healthy — and still be unable to accept a channel range,
/// because only heartbeats open its admin window. A UI that inferred liveness
/// from the last frame of any kind would show a green light next to a node
/// nothing can be assigned to.
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
    /// SSID, lossily decoded — this one is for human eyes, and the store keeps
    /// the original bytes.
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
    /// Every node, ordered by MAC so the table does not reshuffle itself.
    pub nodes: Vec<NodeView>,
    /// How many of them can still be given a channel range.
    pub alive: usize,
    /// The most recent observations, newest last.
    pub tail: Vec<TailEntry>,
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
    /// everything it hears, so this number is large and meaningless the moment
    /// a host connects to a dongle that has been running a while.
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
}

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
            bridge_status: None,
            dropped_baseline: None,
            last_status_poll: None,
            started_at_ms: now.unix_ms,
            pending: BTreeMap::new(),
            next_send_id: 1,
            last_counter: config.assignment_base,
            config,
        }
    }

    /// Advance the state machine. No I/O, no clock reads, no allocation beyond
    /// the batch itself.
    pub fn handle(&mut self, event: Event, now: Now) -> ActionBatch {
        let mut batch = ActionBatch::default();
        match event {
            Event::Tick => self.on_tick(now, &mut batch),
            Event::Command(command) => self.on_command(command, now),
            Event::Link(LinkEvent::Connected(info)) => {
                batch.records.push(Record::Bridge(crate::record::BridgeSeen {
                    mac: info.mac,
                    chip: format!("{:?}", info.chip),
                    fw_version: info.fw_version.clone(),
                }));
                self.bridge = Some(info);
                self.link_up = true;
                self.link_error = None;
                // A new connection means a new baseline, whether or not the
                // bridge itself rebooted.
                self.dropped_baseline = None;
                // Ask straight away rather than waiting out the interval: the
                // first thing an operator wants after a reconnect is evidence
                // the far end is really there.
                self.last_status_poll = Some(now.mono);
                batch.bulk.push(wartui_proto::link::HostToBridge::GetStatus);
            }
            Event::Link(LinkEvent::Disconnected { reason }) => {
                self.link_up = false;
                self.link_error = Some(reason);
            }
            Event::Link(LinkEvent::Garbled(_)) => self.counters.garbled += 1,
            Event::Link(LinkEvent::Message(msg)) => self.on_message(&msg, now, &mut batch),
        }
        batch
    }

    fn on_tick(&mut self, now: Now, batch: &mut ActionBatch) {
        self.expire_pending(now, batch);
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
            // `Ready` reaches the engine as `LinkEvent::Connected`, and the
            // bridge's own diagnostics are the operator's business rather than
            // the engine's.
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

        let Ok(frame) = Frame::decode(payload) else {
            self.counters.undecodable += 1;
            return;
        };

        // Decode before admitting anyone to the fleet. Channel 6 carries
        // whatever else is nearby, and a sender whose frames are not ours is
        // not a node: a vendor core assigning our nodes channels would
        // otherwise sit in the table forever as `no heartbeat`, inflate the
        // "n of m alive" denominator, and leave a `node` row outliving the
        // session it was seen in.
        let text = match frame {
            Frame::Admin(_) => {
                // Nothing to do about it from here, but an operator chasing a
                // fleet that keeps changing its mind needs to know.
                self.counters.foreign_admin += 1;
                return;
            }
            Frame::Text(text) => text,
        };

        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        node.last_seen = now.mono;
        node.last_seen_ms = now.unix_ms;
        node.link_rssi = Some(rssi);
        batch.records.push(Record::Node(NodeSeen {
            mac: src,
            first_seen_ms: node.first_seen_ms,
            last_seen_ms: now.unix_ms,
        }));

        match text.msg_type {
            MsgType::Heartbeat => {
                self.counters.heartbeats += 1;
                let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
                // Divergence 5: the counter runs from the node's boot, so a
                // value below the last one means it restarted and has
                // forgotten whatever range it was assigned. The vendor core
                // has no equivalent check and simply carries on believing
                // its own assignment table.
                let rebooted = node.counter.is_some_and(|previous| text.counter < previous);
                if rebooted {
                    node.reboots += 1;
                    // It has forgotten whatever range it held, and its own
                    // version field went back to its boot value with it. So
                    // the belief goes, and the range is re-issued under a
                    // fresh epoch rather than one the node might now match.
                    node.confirmed = None;
                }
                node.note_beat_gap(now);
                node.counter = Some(text.counter);
                node.last_heartbeat = Some(now.mono);
                node.last_heartbeat_rx_us = Some(rx_us);
                node.heartbeats += 1;
                batch.records.push(Record::Heartbeat(Heartbeat {
                    node_mac: src,
                    rx_at_ms: now.unix_ms,
                    counter: text.counter,
                    link_rssi: Some(rssi),
                }));

                if rebooted && node.desired.is_some() {
                    self.reissue(src);
                }
                // The node is holding its admin window open for the next
                // 300 ms and its radio will be gone after that, so this is
                // the only moment in the sweep worth transmitting in.
                self.send_admin(src, now, batch);
            }
            MsgType::Text => {
                let Ok(line) = WardriveLine::parse(text.text) else {
                    self.counters.unparsed += 1;
                    return;
                };
                self.counters.observations += 1;
                if let Some(node) = self.nodes.get_mut(&src) {
                    node.observations += 1;
                }
                let observation = Observation {
                    node_mac: src,
                    rx_at_ms: now.unix_ms,
                    link_rssi: Some(rssi),
                    bssid: line.bssid,
                    ssid: line.ssid.to_vec(),
                    security: String::from_utf8_lossy(line.security.as_bytes()).into_owned(),
                    channel: line.channel,
                    rssi: line.rssi,
                    kind: line.kind,
                    fix: self.config.position.resolve(),
                    raw_text: text.text.to_vec(),
                };
                self.push_tail(&observation);
                batch.records.push(Record::Observation(observation));
            }
            MsgType::CoreRequest | MsgType::CoreReply => {
                // Only an encrypted node ever sends these, and wartui does
                // not speak encrypted ESP-NOW. Marking the node is what
                // lets the UI say which one to go and reconfigure instead
                // of leaving the operator with an unexplained silence.
                self.counters.core_frames += 1;
                if let Some(node) = self.nodes.get_mut(&src) {
                    node.encrypted = true;
                }
            }
            // Unreachable: `Frame::decode` routes this type byte to
            // `AdminMsg`, which is handled above. Counted rather than ignored
            // so a future change to that dispatch cannot lose frames silently.
            MsgType::Admin => self.counters.foreign_admin += 1,
        }
    }

    /// Take an operator's instruction. Nothing goes out from here.
    fn on_command(&mut self, command: Command, now: Now) {
        let Command::Assign { mac, range } = command;
        // Index and count over every node this session has seen, ordered by
        // MAC. Deterministic, and it does not shuffle the fleet's stagger slots
        // every time one node misses a heartbeat. Phase 5's planner replaces
        // this with a real partition of the channel pool.
        let node_count = u8::try_from(self.nodes.len()).unwrap_or(u8::MAX);
        let Some(node_index) = self.nodes.keys().position(|k| *k == mac) else { return };
        let node_index = u8::try_from(node_index).unwrap_or(u8::MAX);

        // A fresh epoch even when the range is unchanged. A node adopts on
        // `!=`, so re-sending an epoch it already holds is a frame it will
        // acknowledge and then discard — which would look exactly like success.
        self.last_counter += 1;
        let assignment = Assignment { range, node_index, node_count, counter: self.last_counter };

        let Some(node) = self.nodes.get_mut(&mac) else { return };
        node.desired = Some(assignment);
        node.dirty = true;
        let _ = now;
    }

    /// Re-mark a node's assignment for delivery under a new epoch.
    fn reissue(&mut self, mac: Mac) {
        self.last_counter += 1;
        let counter = self.last_counter;
        if let Some(node) = self.nodes.get_mut(&mac)
            && let Some(desired) = node.desired.as_mut()
        {
            desired.counter = counter;
            node.dirty = true;
        }
    }

    /// Put a dirty node's assignment on the air, if it has one.
    ///
    /// Only ever called straight off a heartbeat: that is the one moment the
    /// node's radio is on the control channel and listening.
    fn send_admin(&mut self, mac: Mac, now: Now, batch: &mut ActionBatch) {
        let id = self.next_send_id;
        self.next_send_id = self.next_send_id.wrapping_add(1).max(1);

        let Some(node) = self.nodes.get_mut(&mac) else { return };
        if !node.dirty {
            return;
        }
        let Some(assignment) = node.desired else { return };
        node.admin_attempts += 1;
        let heartbeat_rx_us = node.last_heartbeat_rx_us;

        let msg = AdminMsg {
            assignment_version: wire_version(assignment.counter),
            node_index: assignment.node_index,
            node_count: assignment.node_count,
            start_channel_idx: assignment.range.start,
            end_channel_idx: assignment.range.end,
        };
        // Ten bytes into a 250-byte buffer, so this cannot fail; the encoder
        // returns a fixed-size array precisely so the padding is structural.
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
            // Divergence 6: add if absent and never remove. The vendor core
            // deletes the peer as a side effect of sending
            // (`src/WiFiOps.cpp:672,676`), which is wasteful and races the
            // transmit callback it then ignores anyway.
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
        // An id we do not know is one of ours from before a reconnect, or a
        // reply to something else entirely. Either way there is no assignment
        // to resolve and nothing to write down.
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

        // A full peer table is terminal, not a miss. The radio holds twenty
        // peers and twenty nodes is the entire supported fleet, so there is no
        // later heartbeat at which this becomes possible — retrying would only
        // spend the rest of the capture writing failure rows at heartbeat rate.
        // Give up on what was wanted, and let the view say why.
        if matches!(status, SendStatus::PeerTableFull) {
            self.counters.peer_table_full += 1;
            if let Some(node) = self.nodes.get_mut(&pending.mac) {
                node.dirty = false;
                node.desired = None;
            }
        }

        // Both stamps are the bridge's own microsecond clock, which wraps
        // about every 71 minutes; a wrapping subtraction is correct across it.
        let latency_us = pending.heartbeat_rx_us.map(|rx_us| tx_us.wrapping_sub(rx_us));

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
            // went out. A `SendResult` lost to a garbled frame expires a couple
            // of seconds later — by which time the retry it provoked may
            // already have been acknowledged — so a failure from an attempt the
            // node has moved past says nothing about where the node is now. It
            // still gets its row, because that attempt really did fail; it just
            // does not get to overwrite what landed afterwards.
            let superseded =
                node.confirmed.is_some_and(|c| c.counter >= pending.assignment.counter);
            if acked {
                node.last_outcome = Some(outcome);
                node.last_latency_us = latency_us;
                // Divergence 3: cleared on the MAC-layer acknowledgement, not
                // on a successful enqueue. Unicast ESP-NOW is acknowledged by
                // the receiver's own hardware, so this is the difference
                // between knowing the node has the assignment and hoping.
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
            wire_version: wire_version(pending.assignment.counter),
            node_index: pending.assignment.node_index,
            node_count: pending.assignment.node_count,
            start_idx: pending.assignment.range.start,
            end_idx: pending.assignment.range.end,
            created_at_ms: pending.sent_ms,
            // `Silent` is the case where nothing came back at all, so there
            // is no delivery to stamp: a time here would only be the timeout's
            // own length wearing the look of an answer.
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
            ssid: String::from_utf8_lossy(&observation.ssid).into_owned(),
            security: observation.security.clone(),
            channel: observation.channel,
            rssi: observation.rssi,
            kind: observation.kind,
        });
    }

    /// Whether a node has heartbeated recently enough to hold a channel range.
    #[must_use]
    pub fn is_alive(&self, node: &NodeState, now: Now) -> bool {
        node.last_heartbeat
            .is_some_and(|last| now.mono.duration_since(last) < self.config.topology_timeout)
    }

    /// Build the view the UI renders.
    ///
    /// Taken on a tick rather than per event: the terminal cannot show more
    /// than a few tens of frames a second and an observation burst must not
    /// turn into a burst of redraws.
    #[must_use]
    pub fn snapshot(&self, now: Now, store: StoreStats) -> Snapshot {
        let nodes: Vec<NodeView> = self
            .nodes
            .values()
            .map(|state| NodeView { assignable: self.is_alive(state, now), state: state.clone() })
            .collect();
        let alive = nodes.iter().filter(|n| n.assignable).count();
        Snapshot {
            bridge: self.bridge.clone(),
            link_up: self.link_up,
            link_error: self.link_error.clone(),
            pool: self.config.pool,
            nodes,
            alive,
            tail: self.tail.iter().cloned().collect(),
            counters: self.counters,
            store,
            bridge_status: self.bridge_status,
            started_at_ms: self.started_at_ms,
            now_ms: now.unix_ms,
            position: self.config.position.resolve(),
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
