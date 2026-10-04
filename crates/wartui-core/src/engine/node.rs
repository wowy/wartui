use std::time::Instant;

use wartui_proto::air::{Capabilities, wire_epoch};
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelSet;

use super::Now;
#[cfg(doc)]
use super::{Command, Counters, EngineConfig, FleetEngine};
use crate::record::AdminOutcome;

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
    /// The bridge may hold a peer slot for this node.
    ///
    /// Set when a frame goes to it, cleared when its peer is removed after
    /// [`EngineConfig::topology_timeout`]. Kept across a bridge announcing itself,
    /// which a port reopen does too: removing a peer the bridge has already lost is
    /// harmless.
    pub peered: bool,
    /// What this host wants the node to be scanning.
    pub desired: Option<Assignment>,
    /// What the node acknowledged, which is a different thing. Cleared when it
    /// reboots, because a reboot means it has forgotten.
    pub confirmed: Option<Assignment>,
    /// Whether [`Self::desired`] still needs to be delivered. Set when the plan
    /// changes, when the Bluetooth scan moves and when the node reboots; cleared
    /// only on an acknowledgement, never on a successful enqueue.
    pub dirty: bool,
    /// Whether this node's dedup ring should be cleared on its next admin window.
    ///
    /// Set by [`Command::ClearRing`]. Cleared on the clear's own `AckOk`, on a full
    /// peer table (the same terminal handling an assignment gets), and when the
    /// node reboots — a node that has just booted already holds an empty ring.
    pub clear_dedup_ring: bool,
    /// How many times an assignment has been put on the air for this node.
    pub admin_attempts: u32,
    /// What happened to the most recent attempt.
    pub last_outcome: Option<AdminOutcome>,
    /// Heartbeat-to-transmit-callback microseconds of the most recent
    /// acknowledged assignment, as the bridge measured it.
    pub last_latency_us: Option<u32>,
    /// Bridge-local microsecond stamp of the most recent heartbeat, which is
    /// the near end of that measurement.
    pub(super) last_heartbeat_rx_us: Option<u32>,
    /// The assignment epoch this node's most recent *live* heartbeat reported
    /// it holds, or `0` for none. `None` before any live heartbeat has arrived;
    /// a replayed heartbeat never sets this, since it says what the node held
    /// when it was sent rather than what it holds now. See
    /// [`FleetEngine::air_is_live`].
    pub held_epoch: Option<u8>,
    /// How many times this node's heartbeat has reported an epoch other than
    /// the one it acknowledged: acked but not adopted, and re-sent on the
    /// heartbeat that revealed it.
    pub unadopted: u32,
    /// The `seq` of this node's most recent sighting batch. `None` before its
    /// first, and reset on a detected reboot: the counter restarts at boot,
    /// and a gap across one is history rather than a loss.
    pub last_seq: Option<u16>,
    /// Whether the batch that set `last_seq` arrived live rather than replayed
    /// from the bridge's backlog. A gap after a replayed batch is not counted;
    /// see [`FleetEngine::note_batch_seq`].
    pub(super) last_seq_live: bool,
    /// Bytes of this node's most recent sighting-batch frame, kept only to
    /// recognise a MAC-layer retransmission of it: same `seq`, same bytes.
    /// `None` before its first batch, and reset alongside `last_seq` on a
    /// detected reboot.
    pub(super) last_batch: Option<Vec<u8>>,
    /// Bridge-local microsecond stamp of [`Self::last_batch`], the near end
    /// of [`FleetEngine::is_duplicate_batch`]'s retransmission window. `None`
    /// before its first batch, and reset alongside `last_batch` on a
    /// detected reboot.
    pub(super) last_batch_rx_us: Option<u32>,
    /// Batches lost between this node and the host, counted from gaps in
    /// `seq` that follow a live batch. Each one is everything one dwell or
    /// Bluetooth scan produced, hidden until the node's dedup ring next
    /// refreshes them.
    pub batches_lost: u64,
    /// Batches from this node dropped as MAC-layer retransmissions; see
    /// [`Counters::duplicate_batches`].
    pub duplicate_batches: u64,
    /// The `wifi_dropped` this node's most recent heartbeat carried, the baseline
    /// [`Counters::wifi_dropped`] advances from. `None` before its first heartbeat.
    pub(super) last_wifi_dropped: Option<u16>,
    /// The same for `ble_dropped` and [`Counters::ble_dropped`].
    pub(super) last_ble_dropped: Option<u16>,
}

/// What one node was told to do, and the fleet arithmetic it was computed
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// Which [`wartui_proto::plan::SCAN_CHANNELS`] indices to dwell on.
    ///
    /// Empty exactly when [`ble`](Self::ble) is set, and never otherwise:
    /// Bluetooth is a node's whole job, and an empty set without the flag is the
    /// one thing a node cannot be told, since it would park while this host
    /// believed it was sweeping. [`FleetEngine::replan`] reads both off one value
    /// so they cannot drift.
    pub channels: ChannelSet,
    /// Whether this node's job is Bluetooth, and so whether it sniffs no Wi-Fi.
    ///
    /// Part of the assignment rather than beside it: it travels in the same frame
    /// and is adopted by the same epoch comparison, so treating them separately
    /// would make "acknowledged" ambiguous.
    pub ble: bool,
    /// Wi-Fi transmit power for this node in ESP-IDF quarter-dBm units.
    pub tx_power: i8,
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
            peered: false,
            desired: None,
            confirmed: None,
            dirty: false,
            clear_dedup_ring: false,
            admin_attempts: 0,
            last_outcome: None,
            last_latency_us: None,
            last_heartbeat_rx_us: None,
            held_epoch: None,
            unadopted: 0,
            last_seq: None,
            last_seq_live: false,
            last_batch: None,
            last_batch_rx_us: None,
            batches_lost: 0,
            duplicate_batches: 0,
            last_wifi_dropped: None,
            last_ble_dropped: None,
        }
    }

    /// Whether this node's heartbeat has confirmed it holds what this host
    /// currently cares about: [`Self::desired`] when there is one, since that
    /// is newer than anything acknowledged; [`Self::confirmed`] otherwise, so
    /// a node that departed the plan (`desired` cleared, `confirmed` kept) is
    /// read against what it actually holds rather than nothing at all.
    ///
    /// The stronger fact layered on top of [`Self::confirmed`]: a MAC-layer ack
    /// says the frame was delivered, this says the node actually took it. `false`
    /// with no live heartbeat yet, or while [`Self::held_epoch`] and that
    /// assignment's epoch disagree.
    #[must_use]
    pub fn adopted(&self) -> bool {
        self.desired
            .or(self.confirmed)
            .is_some_and(|d| self.held_epoch == Some(wire_epoch(d.counter)))
    }
}
