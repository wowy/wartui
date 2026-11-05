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
    Band, CHANNEL_SET_BYTES, ChannelInfo, ChannelPool, ChannelSet, ChannelSetIter,
    FIRST_FIVE_GHZ_INDEX, IndexRun, NUM_SCAN_CHANNELS, SCAN_CHANNELS, UNSUPPORTED_INDEX,
    channel_info, is_five_ghz,
};

/// The largest fleet wartui supports.
///
/// Twenty, the peers an ESP-NOW radio can hold. A node the host cannot address is not
/// one it can drive.
pub const MAX_NODES: usize = 20;

/// What the planner is dealing to, one per fleet slot.
///
/// The operator decides it. It reaches [`plan_for`] as one value per slot, so a node dealt
/// nothing for Bluetooth cannot get an assignment without the flag and be told to do
/// nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Sniff Wi-Fi, on whatever the deal gives it.
    Wifi,
    /// Scan Bluetooth, and so be dealt no channels.
    ///
    /// Still a slot: it counts in [`Plan::node_count`], which reports the fleet size the
    /// plan was cut for, not who is sniffing. The shares around it are cut as though it
    /// were present and idle.
    Bluetooth,
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
    /// the first. The rest are dealt nothing and told nothing: a defined outcome for a
    /// call the host never makes.
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
    /// - `None`: nothing for this node. The index is outside the fleet, or it is a
    ///   [`Job::Bluetooth`] slot after the first. A [`Job::Wifi`] slot is never `None`:
    ///   every pool holds at least [`MAX_NODES`] channels.
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
    /// Empty unless no slot is sniffing, which happens only when the whole fleet is the
    /// Bluetooth node. Then it is the whole pool.
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
/// Every node is taken to be sniffing Wi-Fi. For a fleet with a Bluetooth node, use
/// [`plan_for`].
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    let fleet = [Job::Wifi; MAX_NODES];
    plan_for(pool, fleet.get(..usize::from(node_count))?)
}

/// Build a plan distributing `pool` across a fleet of known jobs.
///
/// The same deal as [`plan`], with a [`Job::Bluetooth`] slot left out of it. That slot is
/// counted but not dealt to, so the sniffing slots' shares come out as though it were not
/// there. `crates/wartui/README.md` § "Channel pools" is the operator's account.
///
/// When the whole fleet scans Bluetooth, every channel goes in no share and is reported by
/// [`Plan::unreachable`]. Such a fleet still gets a plan: a refusal would leave the node on
/// its old share, sniffing Wi-Fi, the opposite of what was asked.
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
    // Ties go to the node after the last one dealt to. Every sniffing node is always
    // tied or one behind, so this is the round-robin.
    let mut cursor = 0usize;
    #[allow(clippy::cast_possible_truncation)]
    let bluetooth =
        fleet.iter().position(|job| matches!(job, Job::Bluetooth)).map(|slot| slot as u8);

    for run in pool.runs() {
        for idx in run.start..=run.end {
            let winner = (0..usize::from(node_count))
                .map(|step| (cursor + step) % usize::from(node_count))
                .filter(|node| fleet[*node] == Job::Wifi)
                .min_by_key(|node| slots[*node].len());
            match winner {
                Some(node) => {
                    slots[node].insert(idx);
                    cursor = (node + 1) % usize::from(node_count);
                }
                // Nothing in this fleet is sniffing, so it goes in no share.
                None => unreachable.insert(idx),
            }
        }
    }
    Some(Plan { node_count, slots, bluetooth, unreachable })
}
