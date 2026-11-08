use std::time::Instant;

use wartui_proto::bus::{Capabilities, DieTemp, Slot, Vdd, wire_epoch};
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelSet;
use wartui_proto::reset::ResetCause;

use super::Now;
#[cfg(doc)]
use super::{Command, Counters, EngineConfig, FleetEngine};
use crate::record::AdminOutcome;

/// Everything the engine knows about one node.
#[derive(Debug, Clone)]
pub struct NodeState {
    /// Full six-byte MAC, the node's identity wherever it is plugged in.
    pub mac: Mac,
    /// The node-bus slot its latest heartbeat came from, and so the one this host sends it
    /// anything through. `None` until a heartbeat names it, or once another node's heartbeat
    /// takes the slot. A node without one cannot be sent anything.
    pub slot: Option<Slot>,
    /// Unix milliseconds of the first record in this session.
    pub first_seen_ms: i64,
    /// Unix milliseconds of the most recent record of any kind.
    pub last_seen_ms: i64,
    /// Monotonic time of the most recent record of any kind. Only a heartbeat refreshes
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
    /// The node chip's 3.3 V rail, from its latest heartbeat. [`Vdd::NONE`] before one, or
    /// when that heartbeat's read failed.
    pub vdd: Vdd,
    /// The first [`Vdd`] reading heard from this node, which [`Self::supply_dropped`] compares
    /// against. [`Vdd::NONE`] before one. It survives the node's reboots, since the offset it
    /// stands in for is the chip's.
    pub vdd_baseline: Vdd,
    /// The node's die temperature, from its latest heartbeat. [`DieTemp::NONE`] before one, or
    /// when that heartbeat's read failed.
    pub die_temp: DieTemp,
    /// What the node said it is, from its latest heartbeat, so a reflashed node stops claiming the
    /// old build's features. `None` until a heartbeat arrives. Such a node is left out of the plan
    /// ([`FleetEngine::is_assignable`]).
    pub capabilities: Option<Capabilities>,
    /// The host logged this node leaving the fleet, so its next heartbeat logs it returning.
    pub departed: bool,
    /// A bridge `Status` showed this node's slot absent, and no heartbeat has come since. Out of
    /// the plan until one does ([`FleetEngine::on_bus`]).
    pub off_bus: bool,
    /// What this host wants the node to be scanning.
    pub desired: Option<Assignment>,
    /// What was delivered to the node. Cleared on reboot, because the node has forgotten.
    pub confirmed: Option<Assignment>,
    /// Whether [`Self::desired`] still needs delivering. Set when the plan changes, the Bluetooth
    /// scan moves, the node reboots, or its heartbeat shows a delivered assignment not adopted.
    /// Cleared on delivery, and when the node departs the plan. Never cleared on enqueue.
    pub dirty: bool,
    /// Whether to clear this node's dedup ring on its next heartbeat. Set by
    /// [`Command::ClearRing`]. Cleared once the clear is delivered, and on reboot, which empties
    /// the ring anyway.
    pub clear_dedup_ring: bool,
    /// How many times an assignment has been sent to this node.
    pub admin_attempts: u32,
    /// What happened to the most recent attempt.
    pub last_outcome: Option<AdminOutcome>,
    /// Send-to-delivery microseconds of the latest delivered assignment, by this host's clock.
    pub last_latency_us: Option<u32>,
    /// The epoch this node's latest *live* heartbeat reported holding, `0` for none. A replayed
    /// heartbeat never sets it, since it says what the node held then, not now
    /// ([`FleetEngine::reading_live`]).
    pub held_epoch: Option<u8>,
    /// The cause of this node's latest restart the host saw that was a fault
    /// ([`ResetCause::is_fault`]). Never set by the first heartbeat, whose boot may predate the
    /// run, nor cleared by a later restart that is not a fault: it stays for the run, as a bridge
    /// restart does.
    pub fault_restart: Option<ResetCause>,
    /// How many restarts the host saw were faults. [`Self::fault_restart`] is the latest.
    pub fault_restarts: u32,
    /// Times this node's heartbeat reported an epoch other than the one delivered to it. Each was
    /// re-sent on that heartbeat.
    pub unadopted: u32,
    /// The `seq` of this node's latest sighting batch. Reset on reboot, since `seq` restarts at
    /// boot and a gap across one is not a loss.
    pub last_seq: Option<u16>,
    /// Whether the batch that set `last_seq` arrived live. A gap after a replayed batch is not
    /// counted ([`FleetEngine::note_batch_seq`]).
    pub(super) last_seq_live: bool,
    /// Bytes of this node's latest sighting batch, kept to recognize a resend of it. Reset with
    /// `last_seq` on reboot.
    pub(super) last_batch: Option<Vec<u8>>,
    /// Bridge-local stamp of [`Self::last_batch`], the near end of
    /// [`FleetEngine::is_duplicate_batch`]'s window. Reset with it on reboot.
    pub(super) last_batch_rx_us: Option<u32>,
    /// Batches lost between this node and the host, from `seq` gaps after a live batch. Each is a
    /// whole dwell's or Bluetooth scan's output, hidden until the node's dedup ring refreshes it.
    pub batches_lost: u64,
    /// Batches from this node dropped as resends ([`Counters::duplicate_batches`]).
    pub duplicate_batches: u64,
    /// The `wifi_refused` of this node's latest heartbeat: the baseline [`Counters::wifi_refused`]
    /// advances from.
    pub(super) last_wifi_refused: Option<u16>,
    /// The same for `ble_refused` and [`Counters::ble_refused`].
    pub(super) last_ble_refused: Option<u16>,
}

/// What one node was told to do, and the fleet arithmetic it was computed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// Which [`wartui_proto::plan::SCAN_CHANNELS`] indices to dwell on. Empty exactly when
    /// [`ble`](Self::ble) is set. An empty set without the flag would idle a node while this host
    /// believed it was sweeping. [`FleetEngine::replan`] reads both off one value so they cannot
    /// drift.
    pub channels: ChannelSet,
    /// Whether this node's job is Bluetooth, sniffing no Wi-Fi. Part of the assignment: it travels
    /// in the same message under the same epoch, so "delivered" stays unambiguous.
    pub ble: bool,
    /// The epoch counter it was allocated from: monotonic within a capture, not persisted
    /// (`FleetEngine::last_counter`).
    pub counter: u64,
}

/// How far a node's rail reading must fall below its first, in millivolts, to count as a drop.
///
/// The reading is low by a per-chip offset, so only a change on one node means anything.
/// `crates/wartui/README.md` § "Reading the fleet table" has the measurements. Readings come in
/// 12 mV steps and a healthy node's does not move, so this is well clear of noise.
const SUPPLY_DROP_MV: u16 = 150;
/// A warning on the die, which runs above ambient, so it comes well before the board around it
/// reaches this temperature.
const HOT_DIE_C: i16 = 70;

/// Whether a rail reading of `mv` is a drop from the node's first reading, `baseline_mv`.
pub(crate) const fn supply_dropped_from(baseline_mv: u16, mv: u16) -> bool {
    baseline_mv.saturating_sub(mv) >= SUPPLY_DROP_MV
}

impl NodeState {
    /// A node that has just been heard from for the first time.
    #[must_use]
    pub fn new(mac: Mac, now: Now) -> Self {
        Self {
            mac,
            slot: None,
            first_seen_ms: now.unix_ms,
            last_seen_ms: now.unix_ms,
            last_seen: now.mono,
            last_heartbeat: None,
            counter: None,
            reboots: 0,
            heartbeats: 0,
            observations: 0,
            vdd: Vdd::NONE,
            vdd_baseline: Vdd::NONE,
            die_temp: DieTemp::NONE,
            capabilities: None,
            departed: false,
            off_bus: false,
            desired: None,
            confirmed: None,
            dirty: false,
            clear_dedup_ring: false,
            admin_attempts: 0,
            last_outcome: None,
            last_latency_us: None,
            held_epoch: None,
            fault_restart: None,
            fault_restarts: 0,
            unadopted: 0,
            last_seq: None,
            last_seq_live: false,
            last_batch: None,
            last_batch_rx_us: None,
            batches_lost: 0,
            duplicate_batches: 0,
            last_wifi_refused: None,
            last_ble_refused: None,
        }
    }

    /// Whether this node's heartbeat confirms it holds the assignment this host cares about.
    ///
    /// That is [`Self::desired`], which is newer, or else [`Self::confirmed`], so a node that left
    /// the plan is read against what it holds. Stronger than [`Self::confirmed`]: delivery says
    /// the message arrived, this says the node took it. `false` before a live heartbeat, or while
    /// [`Self::held_epoch`] disagrees.
    #[must_use]
    pub fn adopted(&self) -> bool {
        self.desired
            .or(self.confirmed)
            .is_some_and(|d| self.held_epoch == Some(wire_epoch(d.counter)))
    }

    /// Whether the latest heartbeat's rail reading is at least [`SUPPLY_DROP_MV`] below
    /// [`Self::vdd_baseline`]. `false` without one, and again once the reading recovers.
    #[must_use]
    pub fn supply_dropped(&self) -> bool {
        self.vdd_baseline
            .mv()
            .zip(self.vdd.mv())
            .is_some_and(|(baseline, mv)| supply_dropped_from(baseline, mv))
    }

    /// Whether the latest heartbeat's die reading is at or above [`HOT_DIE_C`]. `false` without
    /// one.
    #[must_use]
    pub fn hot(&self) -> bool {
        self.die_temp.c().is_some_and(|c| c >= HOT_DIE_C)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A `Vdd` moves in 12 mV steps and misses 150 exactly, so the edge is tested in raw millivolts.
    #[test]
    fn supply_drop_trips_when_fall_is_exactly_the_threshold() {
        assert!(supply_dropped_from(3300, 3300 - SUPPLY_DROP_MV));
    }

    #[test]
    fn supply_drop_stays_clear_when_fall_is_one_millivolt_short() {
        assert!(!supply_dropped_from(3300, 3300 - SUPPLY_DROP_MV + 1));
    }

    #[test]
    fn supply_drop_stays_clear_when_reading_rises() {
        assert!(!supply_dropped_from(3000, 3300));
    }
}
