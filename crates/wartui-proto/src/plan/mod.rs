//! Channel pools and the assignment planner.
//!
//! A *pool* is described in runs, because that is the shape regulation gives it: every
//! pool is two runs with a gap between. An *assignment* is not. [`crate::air::AdminMsg`]
//! carries a forty-two-bit [`ChannelSet`], one bit per [`SCAN_CHANNELS`] entry, so a node
//! can hold any subset. A run boundary is nothing the planner steers around.
//!
//! The split is a round-robin deal: index `k` of the pool's flattened order goes to node
//! `k % node_count`. The operator's manual has the rest of the reasoning
//! (`crates/wartui/README.md` § "Channel pools").
//!
//! A fleet is a slice of [`Job`]s rather than of radios, because the node scanning
//! Bluetooth is dealt nothing. It is still a slot, counted in [`Plan::node_count`].

use crate::air::AdminMsg;

mod channels;

pub use channels::{
    CHANNEL_SET_BYTES, ChannelPool, ChannelSet, ChannelSetIter, FIRST_FIVE_GHZ_INDEX, IndexRun,
    NUM_SCAN_CHANNELS, SCAN_CHANNELS, UNSUPPORTED_INDEX, is_five_ghz,
};

/// The largest fleet wartui supports.
///
/// Twenty, the peers an ESP-NOW radio can hold. A node the host cannot address is not
/// one it can drive.
pub const MAX_NODES: usize = 20;

/// What a node's radio can reach.
///
/// The only part of a node's [`crate::air::Capabilities`] the planner may see. Nothing
/// else a node announces should be able to change a plan. What the node is *asked* to
/// do arrives beside it as a [`Job`].
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
/// A node's radio is what it *can* tune, from its capabilities. Its job is what it is
/// *asked* to do, from the operator. Both reach [`plan_for`] as one value per slot, so
/// they cannot drift apart. If they did, a node dealt nothing for Bluetooth could get an
/// assignment without the flag, and be told to do nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Sniff Wi-Fi with this radio, on whatever the deal gives it.
    Wifi(Radio),
    /// Scan Bluetooth, and so be dealt no channels.
    ///
    /// Still a slot: it counts in [`Plan::node_count`], which reports the fleet size the
    /// plan was cut for, not who is sniffing. The shares around it are cut as though it
    /// were present and idle.
    ///
    /// It carries no [`Radio`]. A node scanning Bluetooth changes no plan by changing
    /// band, so reflashing it from a C5 to a C6 causes no re-cut.
    Bluetooth,
}

impl Job {
    /// Whether the node in this slot can be dealt `idx`.
    ///
    /// Always false for [`Self::Bluetooth`]. That is how the planner cuts it out of the
    /// deal.
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
/// One per fleet membership, with no phases and no timer: a mask says everything a node
/// needs to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    node_count: u8,
    slots: [ChannelSet; MAX_NODES],
    /// The slot dealt nothing because it is scanning Bluetooth, if any.
    ///
    /// One, not a set. At most one node scans Bluetooth, and this field makes that
    /// bound structural rather than a rule a caller must keep. A fleet naming two keeps
    /// the first. The rest are surplus slots, dealt nothing and told nothing: a defined
    /// outcome for a call the host never makes.
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
    /// Three answers. The two empty ones mean opposite things:
    ///
    /// - `Some` non-empty: its share of the pool.
    /// - `Some` empty: it is the node scanning Bluetooth. The empty set, sent with
    ///   [`crate::air::ADMIN_FLAG_BLE`], says so on the wire.
    /// - `None`: nothing for this node. Either the index is outside the fleet, or the
    ///   fleet has more nodes than channels they can reach. No frame means "scan
    ///   nothing", so the caller leaves the node holding what it has. A node that
    ///   holds *nothing*, such as one the Bluetooth scan just moved off, needs a share
    ///   all the same; the caller gives it [`ChannelPool::reachable_by`].
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
    /// Two causes, which a caller tells apart from the set itself:
    ///
    /// - **A 2.4 GHz index means no slot is sniffing.** Every [`Radio`] tunes 2.4 GHz,
    ///   so this happens only when the whole fleet is the Bluetooth node.
    /// - **An all-5 GHz set means no sniffing radio reaches 5 GHz.** A lone ESP32-C6
    ///   produces it, and so does an ESP32-C5 holding the scan beside one.
    #[must_use]
    pub const fn unreachable(&self) -> ChannelSet {
        self.unreachable
    }

    /// The assignment to send to slot `index`.
    ///
    /// `epoch` is the caller's persisted epoch byte; a node adopts only when it differs
    /// from the one it holds. `flags` is the caller's too: *which* node scans Bluetooth
    /// is the operator's decision.
    ///
    /// `None` when there is nothing to say. Also `None` for the one frame that must never
    /// exist: an empty [`ChannelSet`] without [`crate::air::ADMIN_FLAG_BLE`]. A node sent
    /// one parks while the host believes it is sweeping.
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
/// The pool's runs are flattened into one ascending list and dealt round-robin: index
/// `k` goes to node `k % node_count`, so every node gets some of every run. Shares differ
/// by at most one channel, so sweep times stay within one dwell of each other.
///
/// Every node is taken to be a dual-band radio sniffing Wi-Fi. For any other fleet, use
/// [`plan_for`].
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    let fleet = [Job::Wifi(Radio::DualBand); MAX_NODES];
    plan_for(pool, fleet.get(..usize::from(node_count))?)
}

/// Build a plan distributing `pool` across a fleet of known radios and jobs.
///
/// The same deal as [`plan`], with two exclusions. Each node is left out of the channels
/// it cannot tune, because 5 GHz cut for an ESP32-C6 is a share nobody scans. A
/// [`Job::Bluetooth`] slot is left out of all of them. Four consequences follow, each
/// deliberate; `crates/wartui/README.md` § "Channel pools" is the operator's account:
///
/// - **In a mixed fleet, 5 GHz is dealt first.** Only the dual-band nodes can take it,
///   so the 2.4 GHz-only nodes are the lightest when 2.4 GHz is dealt. Dealing in pool
///   order instead would give the dual-band nodes a full 2.4 GHz share plus all of
///   5 GHz. A uniform fleet has nothing to deal first, and gets a plan byte-identical
///   to [`plan`]'s.
/// - **Shares can then differ by more than one.** The planner minimizes the *largest*
///   share, which sets how stale the slowest node's sightings get.
/// - **Channels no radio present can tune go in no share**, and are reported by
///   [`Plan::unreachable`]. When the whole fleet scans Bluetooth, that is every channel.
///   Such a fleet still gets a plan: a refusal would leave the node on its old share,
///   sniffing Wi-Fi, the opposite of what was asked.
/// - **A Bluetooth slot is counted but not dealt to.** See [`Job::Bluetooth`].
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
    // Over the sniffing slots only. A C5 holding the Bluetooth scan beside a C6 is
    // not a mixed fleet: it is a one-radio fleet with 5 GHz out of reach.
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
