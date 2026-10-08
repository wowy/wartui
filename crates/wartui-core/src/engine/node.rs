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
    /// Monotonic time of the most recent frame of any kind. Only a heartbeat refreshes
    /// `last_heartbeat`, which answers a different question.
    pub last_seen: Instant,
    /// Monotonic time of the most recent heartbeat, which decides whether the node can be assigned.
    pub last_heartbeat: Option<Instant>,
    /// The node's most recent heartbeat counter.
    pub counter: Option<u32>,
    /// Reboots detected: the heartbeat counter went backwards, or a live heartbeat's epoch fell
    /// back to 0 after a real one. Each forgets the assignment held.
    pub reboots: u32,
    /// Heartbeats received in this session.
    pub heartbeats: u64,
    /// Observations received in this session.
    pub observations: u64,
    /// Most recent link RSSI, as the bridge measured it.
    pub link_rssi: Option<i8>,
    /// What the node said it is, from its latest heartbeat, so a reflashed node stops claiming the
    /// old build's features. `None` until a heartbeat arrives, because a node reports a channel's
    /// sightings before returning to the control channel. Such a node is left out of the plan
    /// ([`FleetEngine::is_assignable`]).
    pub capabilities: Option<Capabilities>,
    /// The bridge's peer table had no room for this node, so it cannot be sent anything and is left
    /// out of the plan ([`FleetEngine::is_assignable`]). Cleared when a bridge announces itself,
    /// since its table starts empty.
    pub peer_refused: bool,
    /// The bridge may hold a peer slot for this node. Set when a frame goes to it, cleared when the
    /// peer is removed after [`EngineConfig::topology_timeout`]. Kept across a bridge announcing
    /// itself, as on a port reopen: removing a peer the bridge already lost is harmless.
    pub peered: bool,
    /// What this host wants the node to be scanning.
    pub desired: Option<Assignment>,
    /// What the node acknowledged. Cleared on reboot, because the node has forgotten.
    pub confirmed: Option<Assignment>,
    /// Whether [`Self::desired`] still needs delivering. Set when the plan changes, the Bluetooth
    /// scan moves, the node reboots, or its heartbeat shows an acked assignment not adopted.
    /// Cleared on an acknowledgement, on a full peer table, and when the node departs the plan.
    /// Never cleared on enqueue.
    pub dirty: bool,
    /// Whether to clear this node's dedup ring in its next admin window. Set by
    /// [`Command::ClearRing`]. Cleared on the clear's `AckOk`, on a full peer table (as for an
    /// assignment), and on reboot, which empties the ring anyway.
    pub clear_dedup_ring: bool,
    /// How many times an assignment has been put on the air for this node.
    pub admin_attempts: u32,
    /// What happened to the most recent attempt.
    pub last_outcome: Option<AdminOutcome>,
    /// Heartbeat-to-transmit-callback microseconds of the latest acknowledged assignment, by the
    /// bridge's clock.
    pub last_latency_us: Option<u32>,
    /// Bridge-local microsecond stamp of the latest heartbeat: the near end of
    /// [`Self::last_latency_us`].
    pub(super) last_heartbeat_rx_us: Option<u32>,
    /// The epoch this node's latest *live* heartbeat reported holding, `0` for none. A replayed
    /// heartbeat never sets it, since it says what the node held then, not now
    /// ([`FleetEngine::air_is_live`]).
    pub held_epoch: Option<u8>,
    /// Times this node's heartbeat reported an epoch other than the one it acknowledged. Each was
    /// re-sent on that heartbeat.
    pub unadopted: u32,
    /// The `seq` of this node's latest sighting batch. Reset on reboot, since `seq` restarts at
    /// boot and a gap across one is not a loss.
    pub last_seq: Option<u16>,
    /// Whether the batch that set `last_seq` arrived live. A gap after a replayed batch is not
    /// counted ([`FleetEngine::note_batch_seq`]).
    pub(super) last_seq_live: bool,
    /// Bytes of this node's latest sighting batch, kept to recognize a MAC-layer retransmission of
    /// it. Reset with `last_seq` on reboot.
    pub(super) last_batch: Option<Vec<u8>>,
    /// Bridge-local stamp of [`Self::last_batch`], the near end of
    /// [`FleetEngine::is_duplicate_batch`]'s window. Reset with it on reboot.
    pub(super) last_batch_rx_us: Option<u32>,
    /// Batches lost between this node and the host, from `seq` gaps after a live batch. Each is a
    /// whole dwell's or Bluetooth scan's output, hidden until the node's dedup ring refreshes it.
    pub batches_lost: u64,
    /// Batches from this node dropped as retransmissions ([`Counters::duplicate_batches`]).
    pub duplicate_batches: u64,
    /// The `wifi_dropped` of this node's latest heartbeat: the baseline [`Counters::wifi_dropped`]
    /// advances from.
    pub(super) last_wifi_dropped: Option<u16>,
    /// The same for `ble_dropped` and [`Counters::ble_dropped`].
    pub(super) last_ble_dropped: Option<u16>,
}

/// What one node was told to do, and the fleet arithmetic it was computed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// Which [`wartui_proto::plan::SCAN_CHANNELS`] indices to dwell on. Empty exactly when
    /// [`ble`](Self::ble) is set. An empty set without the flag would park a node while this host
    /// believed it was sweeping. [`FleetEngine::replan`] reads both off one value so they cannot
    /// drift.
    pub channels: ChannelSet,
    /// Whether this node's job is Bluetooth, sniffing no Wi-Fi. Part of the assignment: it travels
    /// in the same frame under the same epoch, so "acknowledged" stays unambiguous.
    pub ble: bool,
    /// Wi-Fi transmit power for this node in ESP-IDF quarter-dBm units.
    pub tx_power: i8,
    /// The epoch counter it was allocated from: monotonic within a capture, not persisted
    /// (`FleetEngine::last_counter`).
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

    /// Whether this node's heartbeat confirms it holds the assignment this host cares about.
    ///
    /// That is [`Self::desired`], which is newer, or else [`Self::confirmed`], so a node that left
    /// the plan is read against what it holds. Stronger than [`Self::confirmed`]: an ack says the
    /// frame arrived, this says the node took it. `false` before a live heartbeat, or while
    /// [`Self::held_epoch`] disagrees.
    #[must_use]
    pub fn adopted(&self) -> bool {
        self.desired
            .or(self.confirmed)
            .is_some_and(|d| self.held_epoch == Some(wire_epoch(d.counter)))
    }
}
