//! Channel pools, and the distribution math that cuts one across a fleet.
//!
//! [`crate::fleet`] is the planner: it decides who is in the fleet, which slot scans Bluetooth and
//! which channels are dealt, and calls [`plan_for`] and [`Plan::assign_for`] to cut them. Nothing
//! here holds state.
//!
//! A *pool* is described in runs, because that is the shape regulation gives it: every
//! pool is two runs with a gap between. An *assignment* is not. [`crate::bus::Assign`]
//! carries a forty-two-bit [`ChannelSet`], one bit per [`SCAN_CHANNELS`] entry, so a node
//! can hold any subset. A run boundary is nothing the planner steers around, and [`plan_for`]
//! deals a [`ChannelSet`]: a pool's channels, or a phone's list.
//!
//! **The deal is one order, cycled.** Channels 1, 6 and 11 come first, those the set holds, then
//! the rest ascending. Position `p` goes to the `p`-th Wi-Fi slot in turn, and the deal runs for
//! as many positions as there are channels or Wi-Fi slots, whichever is more.
//!
//! - 1, 6 and 11 are the busiest channels, so they land on three different nodes, and with three
//!   or more Wi-Fi slots no node carries two of them.
//! - With at least as many channels as Wi-Fi slots, each channel is dealt once and shares differ
//!   by at most one, so sweep times stay within one dwell of each other.
//! - With fewer, the order starts over, so every Wi-Fi slot holds one channel and 1, 6 and 11 are
//!   the first held twice.
//!
//! The operator's manual has the rest of the reasoning (`crates/wartui/README.md` § "Channel
//! pools").
//!
//! A fleet is a slice of [`Job`]s rather than of radios, because the node scanning
//! Bluetooth is dealt nothing. It is still a slot, counted in [`Plan::node_count`].

use crate::bus::{ASSIGN_FLAG_BLE, Assign, MAX_SLOTS};

mod channels;

pub use channels::{
    Band, CHANNEL_SET_BYTES, ChannelInfo, ChannelPool, ChannelSet, ChannelSetIter,
    FIRST_FIVE_GHZ_INDEX, IndexRun, NUM_SCAN_CHANNELS, SCAN_CHANNELS, UNSUPPORTED_INDEX,
    channel_info, index_of, is_five_ghz,
};

/// The largest fleet wartui supports: the bridge's node-bus slots. A node in no slot is not
/// one the bridge can drive.
pub const MAX_NODES: usize = MAX_SLOTS;

/// The [`SCAN_CHANNELS`] indices of channels 1, 6 and 11, which the deal places first.
const POPULAR: [u8; 3] = [0, 5, 10];

const _: () = assert!(
    SCAN_CHANNELS[POPULAR[0] as usize] == 1
        && SCAN_CHANNELS[POPULAR[1] as usize] == 6
        && SCAN_CHANNELS[POPULAR[2] as usize] == 11,
    "POPULAR must still be channels 1, 6 and 11"
);

/// What the planner is dealing to, one per fleet slot.
///
/// The fleet planner decides it ([`crate::fleet::pick_bluetooth`]). It reaches [`plan_for`] as
/// one value per slot, so a node dealt
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
    /// call the fleet planner never makes.
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
    /// - `Some` non-empty: its share of the channels dealt.
    /// - `Some` empty: it is the node scanning Bluetooth. The empty set, sent with
    ///   [`ASSIGN_FLAG_BLE`], says so on the wire.
    /// - `None`: nothing for this node. The index is outside the fleet, or it is a
    ///   [`Job::Bluetooth`] slot after the first. A [`Job::Wifi`] slot is never `None` while
    ///   anything was dealt: a set smaller than the fleet is dealt again.
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

    /// Channels dealt that went into no share, and that no amount of waiting will fill.
    ///
    /// Empty unless no slot is sniffing, which happens only when the whole fleet is the
    /// Bluetooth node. Then it is every channel dealt.
    #[must_use]
    pub const fn unreachable(&self) -> ChannelSet {
        self.unreachable
    }

    /// The assignment to send to slot `index`.
    ///
    /// `epoch` is the caller's persisted epoch byte; a node adopts only when it differs
    /// from the one it holds. `flags` is the caller's too: *which* node scans Bluetooth
    /// is the fleet planner's decision.
    ///
    /// `None` when there is nothing to say. Also `None` for the one assignment that must never
    /// exist: an empty [`ChannelSet`] without [`ASSIGN_FLAG_BLE`]. A node sent
    /// one parks while the fleet believes it is sweeping.
    #[must_use]
    pub fn assign_for(&self, index: u8, epoch: u8, flags: u8) -> Option<Assign> {
        let channels = self.channels_for(index)?;
        if channels.is_empty() && flags & ASSIGN_FLAG_BLE == 0 {
            return None;
        }
        Some(Assign { epoch, flags, channels })
    }
}

/// Build a plan distributing `pool` across `node_count` nodes.
///
/// The deal is [`plan_for`]'s, over the pool's channels: 1, 6 and 11 first, then the rest
/// ascending, so every node gets some of both bands when each band has a channel for every node.
/// Every pool holds more channels than [`MAX_NODES`], so each is dealt once and shares differ by
/// at most one channel.
///
/// Every node is taken to be sniffing Wi-Fi. For a fleet with a Bluetooth node, use
/// [`plan_for`].
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    let fleet = [Job::Wifi; MAX_NODES];
    plan_for(pool.channels(), fleet.get(..usize::from(node_count))?)
}

/// Build a plan distributing `channels` across a fleet of known jobs.
///
/// The deal order is channels 1, 6 and 11, those `channels` holds, then the rest ascending.
/// Position `p` of it goes to the `p`-th [`Job::Wifi`] slot in turn, in slot order, wrapping. So
/// the busiest channels sit on different nodes, and with three or more Wi-Fi slots no node carries
/// two of them. The deal runs for as many positions as there are channels or Wi-Fi slots,
/// whichever is more, starting the order over when it runs out: a set smaller than the fleet is
/// dealt again, popular first, so every Wi-Fi slot holds a channel.
///
/// A [`Job::Bluetooth`] slot is counted but not dealt to, so the sniffing slots' shares come out
/// as though it were not there. `crates/wartui/README.md` § "Channel pools" is the operator's
/// account.
///
/// When the whole fleet scans Bluetooth, every channel goes in no share and is reported by
/// [`Plan::unreachable`]. Such a fleet still gets a plan: a refusal would leave the node on
/// its old share, sniffing Wi-Fi, the opposite of what was asked.
///
/// Returns `None` for an empty fleet, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan_for(channels: ChannelSet, fleet: &[Job]) -> Option<Plan> {
    let node_count = u8::try_from(fleet.len()).ok()?;
    if node_count == 0 || fleet.len() > MAX_NODES {
        return None;
    }
    let mut slots = [ChannelSet::empty(); MAX_NODES];
    let mut unreachable = ChannelSet::empty();
    #[allow(clippy::cast_possible_truncation)]
    let bluetooth =
        fleet.iter().position(|job| matches!(job, Job::Bluetooth)).map(|slot| slot as u8);

    let wifi = || (0..fleet.len()).filter(|node| fleet[*node] == Job::Wifi);
    let wifi_count = wifi().count();
    if wifi_count == 0 {
        // Nothing in this fleet is sniffing, so every channel goes in no share.
        unreachable = channels;
    } else {
        let popular = POPULAR.into_iter().filter(move |idx| channels.contains(*idx));
        let rest = channels.indices().filter(|idx| !POPULAR.contains(idx));
        // `len` is at most `NUM_SCAN_CHANNELS`.
        let positions = (channels.len() as usize).max(wifi_count);
        for (idx, node) in popular.chain(rest).cycle().zip(wifi().cycle()).take(positions) {
            slots[node].insert(idx);
        }
    }
    Some(Plan { node_count, slots, bluetooth, unreachable })
}
