//! Channel pools and the assignment planner.
//!
//! A *pool* is described in runs — every one of them is two, each with its own
//! hole in the middle — because that is the shape the regulatory picture has. An
//! *assignment* is not: [`crate::air::AdminMsg`] carries a forty-two-bit
//! [`ChannelSet`], one bit per [`SCAN_CHANNELS`] entry, so a node can hold any
//! subset and a run boundary is nothing the planner steers around. Before the
//! mask a lone node on a two-run pool had to rotate between them on a timer.
//!
//! The split is a round-robin deal: index `k` of the pool's flattened order goes
//! to node `k % node_count`. The operator's manual has the rest of the reasoning
//! (`crates/wartui/README.md` § "Channel pools").
//!
//! A fleet is a slice of [`Job`]s rather than of radios, because one node's job is
//! Bluetooth and that node is dealt nothing. It is still a slot, counted in
//! [`Plan::node_count`] for the fleet table, so cutting it out of the deal must not
//! cut it out of the count.

use crate::air::AdminMsg;

mod channels;

pub use channels::{
    CHANNEL_SET_BYTES, ChannelPool, ChannelSet, ChannelSetIter, FIRST_FIVE_GHZ_INDEX, IndexRun,
    NUM_SCAN_CHANNELS, SCAN_CHANNELS, UNSUPPORTED_INDEX, is_five_ghz,
};

/// The largest fleet wartui supports.
///
/// Twenty, because that is how many peers an ESP-NOW radio can hold, and a node
/// this host cannot address is not one it can drive.
pub const MAX_NODES: usize = 20;

/// What a node's radio can reach.
///
/// The only part of a node's capability token the planner may look at, and
/// deliberately not [`crate::air::Capabilities`] itself: nothing else a node
/// announces should be able to change a plan. What the node has been *asked* to do
/// arrives beside it as a [`Job`], which is the operator's decision rather than the
/// token's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Radio {
    /// 2.4 and 5 GHz — an ESP32-C5.
    #[default]
    DualBand,
    /// 2.4 GHz only — an ESP32-C6.
    TwoPointFour,
}

impl Radio {
    /// Whether this radio can tune `idx`.
    #[must_use]
    pub const fn can_tune(self, idx: u8) -> bool {
        matches!(self, Self::DualBand) || !is_five_ghz(idx)
    }
}

impl From<crate::air::Capabilities> for Radio {
    fn from(capabilities: crate::air::Capabilities) -> Self {
        if capabilities.five_ghz { Self::DualBand } else { Self::TwoPointFour }
    }
}

/// What the planner is dealing to, one per fleet slot.
///
/// A node's radio is what it *can* tune and comes off its own capability token; the
/// job is what it has been *asked* to do and comes from the operator. Both reach
/// [`plan_for`] as one value per slot so the two cannot be held separately and drift
/// apart — a node dealt no channels because it is scanning Bluetooth, and an
/// assignment whose flag says otherwise, is a node told to do nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Sniff Wi-Fi with this radio, on whatever the deal gives it.
    Wifi(Radio),
    /// Scan Bluetooth, and so be dealt no channels.
    ///
    /// Still a slot: it counts in [`Plan::node_count`], which reports the fleet size
    /// the plan was cut for rather than a description of who is sniffing. The
    /// shares around it are cut as though it were present and idle.
    ///
    /// No [`Radio`], deliberately. A node scanning Bluetooth changes no plan by
    /// changing band, so a reflash from a C5 to a C6 while it holds the scan
    /// correctly causes no re-cut.
    Bluetooth,
}

impl Job {
    /// Whether the node in this slot can be dealt `idx`.
    ///
    /// Always false for [`Self::Bluetooth`], which is the whole of how the planner
    /// cuts it out of the deal.
    #[must_use]
    pub const fn can_tune(self, idx: u8) -> bool {
        match self {
            Self::Wifi(radio) => radio.can_tune(idx),
            Self::Bluetooth => false,
        }
    }

    /// The radio this slot sniffs with, or `None` when it is not sniffing.
    #[must_use]
    pub const fn radio(self) -> Option<Radio> {
        match self {
            Self::Wifi(radio) => Some(radio),
            Self::Bluetooth => None,
        }
    }
}

impl From<Radio> for Job {
    fn from(radio: Radio) -> Self {
        Self::Wifi(radio)
    }
}

/// A fleet-wide assignment: one [`ChannelSet`] per node.
///
/// Exactly one of these per fleet membership, with no phases and no timer behind
/// it: a mask says everything a node needs to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    node_count: u8,
    slots: [ChannelSet; MAX_NODES],
    /// The slot dealt nothing because it is scanning Bluetooth, if any.
    ///
    /// One, not a set: at most one node scans Bluetooth, and this is where that
    /// bound becomes structural rather than a rule a caller has to keep. A fleet
    /// naming two keeps the first and the rest are surplus slots, dealt nothing and
    /// told nothing, which is a defined outcome for a call the host cannot make.
    bluetooth: Option<u8>,
    unreachable: ChannelSet,
}

impl Plan {
    /// Number of nodes the plan was built for.
    #[must_use]
    pub const fn node_count(&self) -> u8 {
        self.node_count
    }

    /// What slot `index` should scan, if anything.
    ///
    /// Three answers in two shapes, because the two empty ones mean opposite
    /// things:
    ///
    /// - `Some` non-empty — its share of the pool.
    /// - `Some` empty — it is the node scanning Bluetooth, and the empty set is
    ///   what says so on the wire. Sent, with [`crate::air::ADMIN_FLAG_BLE`].
    /// - `None` — nothing for this node: an index outside the fleet, or a surplus
    ///   slot in a fleet with more nodes than it has channels it can reach. There
    ///   is no frame meaning "scan nothing", so a caller that gets `None` leaves
    ///   the node holding whatever it already has rather than inventing one — and
    ///   has to answer for the node holding *nothing*, which is the Bluetooth
    ///   scanner's share after the scan is taken off it. [`ChannelPool::reachable_by`]
    ///   is what that caller reaches for.
    #[must_use]
    pub fn channels_for(&self, index: u8) -> Option<ChannelSet> {
        let set = *self.slots.get(usize::from(index))?;
        if index >= self.node_count {
            return None;
        }
        if self.bluetooth == Some(index) {
            return Some(ChannelSet::empty());
        }
        (!set.is_empty()).then_some(set)
    }

    /// Which slot is scanning Bluetooth, if any.
    #[must_use]
    pub const fn bluetooth(&self) -> Option<u8> {
        self.bluetooth
    }

    /// Channels of the pool that went into no share, and that no amount of waiting
    /// will fill.
    ///
    /// Two causes, and a caller can tell them apart without asking: every [`Radio`]
    /// tunes 2.4 GHz, so a 2.4 GHz index is in here only when no slot is sniffing at
    /// all — a fleet whose whole membership is the Bluetooth scanner. An all-5 GHz
    /// set is the other cause, the pool asking for a band nothing present can reach,
    /// which a lone ESP32-C6 produces and so does an ESP32-C5 that holds the scan
    /// beside one.
    #[must_use]
    pub const fn unreachable(&self) -> ChannelSet {
        self.unreachable
    }

    /// The assignment to send to slot `index`.
    ///
    /// `epoch` is the caller's persisted epoch byte; a node adopts only when it
    /// differs from the one it holds. `flags` is the caller's too: *which* node scans
    /// Bluetooth is the operator's decision, and the plan is what acts on it.
    ///
    /// `None` when there is nothing to say, and also for the one frame that must
    /// never exist: an empty [`ChannelSet`] without
    /// [`crate::air::ADMIN_FLAG_BLE`] tells a node to scan nothing, and a node sent
    /// one parks while the host goes on believing it is sweeping.
    #[must_use]
    pub fn admin_for(&self, index: u8, epoch: u8, flags: u8, tx_power: i8) -> Option<AdminMsg> {
        let channels = self.channels_for(index)?;
        if channels.is_empty() && flags & crate::air::ADMIN_FLAG_BLE == 0 {
            return None;
        }
        Some(AdminMsg { epoch, flags, channels, tx_power })
    }
}

/// Build a plan distributing `pool` across `node_count` nodes.
///
/// The pool's runs are flattened into one ascending list and dealt round-robin:
/// index `k` goes to node `k % node_count`, so every node gets some of every run.
/// Shares differ by at most one channel, so heartbeat periods stay within one
/// dwell of each other.
///
/// Every node is taken to be a dual-band radio sniffing Wi-Fi; for a fleet that is
/// not, use [`plan_for`].
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    let fleet = [Job::Wifi(Radio::DualBand); MAX_NODES];
    plan_for(pool, fleet.get(..usize::from(node_count))?)
}

/// Build a plan distributing `pool` across a fleet of known radios and jobs.
///
/// The same deal as [`plan`], with each node excluded from the channels it cannot
/// tune, because a share of 5 GHz cut for an ESP32-C6 is a share nobody scans, and
/// a [`Job::Bluetooth`] slot excluded from all of them, because Bluetooth is the
/// whole of what that node was asked for. Four consequences, each deliberate and
/// none obvious — the operator-facing account is `crates/wartui/README.md`
/// § "Channel pools":
///
/// - **The constrained channels go round first in a mixed fleet**, so the nodes
///   that sat them out are the lightest when the rest is dealt. In pool order the
///   dual-band nodes take their 2.4 GHz share and then all of 5 GHz on top, which
///   is the block split arrived at sideways. A uniform fleet has no constrained
///   channels and gets a plan byte-identical to [`plan`]'s.
/// - **Shares are then no longer within one of each other**, and cannot be. What
///   is minimised is the *largest* share, which sets how stale the slowest node's
///   observations get.
/// - Channels no radio present can tune are left out of every share and reported
///   by [`Plan::unreachable`], and so are all of them when the fleet's whole
///   membership is scanning Bluetooth. A fleet of nothing but Bluetooth is a plan
///   rather than a refusal: refusing would leave that node holding its old share
///   and still sniffing Wi-Fi, which is the opposite of what was asked.
/// - **A Bluetooth slot is counted but not dealt to.** [`Plan::node_count`]
///   includes it, because that reports the fleet's shape rather than who is
///   sniffing; see [`Job::Bluetooth`].
///
/// Returns `None` for an empty fleet, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan_for(pool: ChannelPool, fleet: &[Job]) -> Option<Plan> {
    let node_count = u8::try_from(fleet.len()).ok()?;
    if node_count == 0 || fleet.len() > MAX_NODES {
        return None;
    }
    let mut slots = [ChannelSet::empty(); MAX_NODES];
    let mut unreachable = ChannelSet::empty();
    // Ties go to the node after the last one dealt to. In a uniform fleet every
    // node is always tied, so this alone is the round-robin.
    let mut cursor = 0usize;
    // Over the sniffing slots only: a C5 holding the Bluetooth scan beside a C6 is
    // not a mixed fleet, it is a one-radio fleet with 5 GHz out of reach.
    let mixed = fleet.contains(&Job::Wifi(Radio::TwoPointFour))
        && fleet.contains(&Job::Wifi(Radio::DualBand));
    #[allow(clippy::cast_possible_truncation)]
    let bluetooth =
        fleet.iter().position(|job| matches!(job, Job::Bluetooth)).map(|slot| slot as u8);

    for pass in 0..2 {
        for run in pool.runs() {
            for idx in run.start..=run.end {
                // One pass unless the fleet is mixed, in which case 5 GHz goes
                // round before the part everyone can take.
                if mixed && is_five_ghz(idx) != (pass == 0) {
                    continue;
                }
                if !mixed && pass == 1 {
                    continue;
                }
                // Least-loaded first, so a node that sat out the 5 GHz pass is
                // ahead of one that did not; ties in fleet order from `cursor`.
                let winner = (0..usize::from(node_count))
                    .map(|step| (cursor + step) % usize::from(node_count))
                    .filter(|node| fleet[*node].can_tune(idx))
                    .min_by_key(|node| slots[*node].len());
                match winner {
                    Some(node) => {
                        slots[node].insert(idx);
                        cursor = (node + 1) % usize::from(node_count);
                    }
                    // Nothing in this fleet is both sniffing and able to reach it,
                    // so it goes in no share.
                    None => unreachable.insert(idx),
                }
            }
        }
    }
    Some(Plan { node_count, slots, bluetooth, unreachable })
}
