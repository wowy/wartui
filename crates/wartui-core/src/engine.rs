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
//! This build is receive-only: [`ActionBatch::urgent`] is always empty because
//! nothing here transmits yet. The channel is there because Phase 4's
//! assignments go down it, and because leaving the shape right costs nothing
//! now and a refactor later.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_proto::air::{Frame, MsgType, RecordKind, WardriveLine};
use wartui_proto::link::{BridgeToHost, Mac};
use wartui_proto::plan::ChannelPool;

use crate::position::PositionChain;
use crate::record::{Heartbeat, NodeSeen, Observation, RawFrame, Record};

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
}

/// What the engine wants done as a result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActionBatch {
    /// Rows for the store, in the order they should be written.
    pub records: Vec<Record>,
    /// Commands that can wait behind anything else.
    pub bulk: Vec<wartui_proto::link::HostToBridge>,
    /// Commands racing a node's 300 ms admin window. Always empty until
    /// Phase 4 enables transmit.
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
}

impl NodeState {
    fn new(mac: Mac, now: Now) -> Self {
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
        }
    }
}

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
}

impl FleetEngine {
    /// Start an engine. `now` fixes the session's start time.
    #[must_use]
    pub fn new(config: EngineConfig, now: Now) -> Self {
        Self {
            config,
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
        }
    }

    /// Advance the state machine. No I/O, no clock reads, no allocation beyond
    /// the batch itself.
    pub fn handle(&mut self, event: Event, now: Now) -> ActionBatch {
        let mut batch = ActionBatch::default();
        match event {
            Event::Tick => self.on_tick(now, &mut batch),
            Event::Link(LinkEvent::Connected(info)) => {
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
            BridgeToHost::Rx { src, dst, rssi, channel, payload, .. } => {
                self.on_rx(*src, *dst, *rssi, *channel, payload, now, batch);
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
            // Nothing here transmits yet, and the bridge's own diagnostics are
            // the operator's business rather than the engine's.
            BridgeToHost::Ready { .. }
            | BridgeToHost::SendResult { .. }
            | BridgeToHost::Log { .. }
            | BridgeToHost::Error { .. } => {}
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

        let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
        node.last_seen = now.mono;
        node.last_seen_ms = now.unix_ms;
        node.link_rssi = Some(rssi);
        batch.records.push(Record::Node(NodeSeen {
            mac: src,
            first_seen_ms: node.first_seen_ms,
            last_seen_ms: now.unix_ms,
        }));

        let Ok(frame) = Frame::decode(payload) else {
            self.counters.undecodable += 1;
            return;
        };

        match frame {
            Frame::Admin(_) => {
                // Somebody else's core is on the air assigning our nodes
                // channels. Nothing to do about it from here, but an operator
                // chasing a fleet that keeps changing its mind needs to know.
                self.counters.foreign_admin += 1;
            }
            Frame::Text(text) => match text.msg_type {
                MsgType::Heartbeat => {
                    self.counters.heartbeats += 1;
                    let node = self.nodes.entry(src).or_insert_with(|| NodeState::new(src, now));
                    // Divergence 5: the counter runs from the node's boot, so a
                    // value below the last one means it restarted and has
                    // forgotten whatever range it was assigned. The vendor core
                    // has no equivalent check and simply carries on believing
                    // its own assignment table.
                    if node.counter.is_some_and(|previous| text.counter < previous) {
                        node.reboots += 1;
                    }
                    node.counter = Some(text.counter);
                    node.last_heartbeat = Some(now.mono);
                    node.heartbeats += 1;
                    batch.records.push(Record::Heartbeat(Heartbeat {
                        node_mac: src,
                        rx_at_ms: now.unix_ms,
                        counter: text.counter,
                        link_rssi: Some(rssi),
                    }));
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
                MsgType::Admin => self.counters.foreign_admin += 1,
            },
        }
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
