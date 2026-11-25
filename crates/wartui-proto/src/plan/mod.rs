//! Channel pools, and the deals that cut a set of channels across a fleet.
//!
//! [`crate::fleet`] is the planner: it decides who is in the fleet, which slot scans Bluetooth and
//! which channels are dealt, and calls [`plan_with`] and [`Plan::assign_for`] to cut them. Nothing
//! here holds state.
//!
//! A *pool* is described in runs, because that is the shape regulation gives it: every
//! pool is two runs with a gap between. An *assignment* is not. [`crate::bus::Assign`]
//! carries a forty-two-bit [`ChannelSet`], one bit per [`SCAN_CHANNELS`] entry, so a node
//! can hold any subset. A run boundary is nothing the planner steers around, and [`plan_with`]
//! deals a [`ChannelSet`]: a pool's channels, or a phone's list.
//!
//! **A [`Dealer`] cuts the set into one share per Wi-Fi slot**, and each deal is its own type, so
//! each is tested alone and all of them pass one contract suite (`tests/planner.rs`):
//!
//! - [`Even`] deals one order, 1, 6 and 11 first, round-robin, so shares differ by at most one
//!   channel and sweep times stay within one dwell of each other.
//! - [`Weighted`] deals the quiet 5 GHz channels to a few sparse nodes and the rest round-robin,
//!   so the busy channels are visited more often. It deals as [`Even`] where weighting cannot
//!   apply: a small fleet, or a set with no quiet channel or too few busy ones.
//!
//! [`Planner`] names the deal, and [`PlanSettings`] carries it with its [`SparseNodes`] count as
//! the one value the fleet deals through.
//!
//! **[`plan_with`] keeps what no dealer may get wrong.** It checks the fleet size, places the
//! Bluetooth slot, and maps the shares onto the Wi-Fi slots in slot order. It masks every share to
//! the set, so no dealer names a channel outside it, and reports what no share holds as
//! [`Plan::unreachable`], so a channel a dealer drops shows in the footer rather than vanishing.
//!
//! In both deals 1, 6 and 11, the busiest channels, take the first three shares, so with three or
//! more Wi-Fi slots no node carries two of them, and every Wi-Fi slot holds a channel, the set
//! dealt again when it is smaller than the fleet.
//!
//! The operator's manual has the rest of the reasoning (`crates/wartui/README.md` § "Channel
//! pools").
//!
//! A fleet is a slice of [`Job`]s rather than of radios, because the node scanning
//! Bluetooth is dealt nothing. It is still a slot, counted in [`Plan::node_count`].

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::bus::{ASSIGN_FLAG_BLE, Assign, MAX_SLOTS};

mod channels;
mod even;
mod weighted;

pub use channels::{
    BUSY_FIVE_GHZ, Band, CHANNEL_SET_BYTES, ChannelInfo, ChannelPool, ChannelSet, ChannelSetIter,
    FIRST_FIVE_GHZ_INDEX, IndexRun, NUM_SCAN_CHANNELS, SCAN_CHANNELS, UNSUPPORTED_INDEX,
    channel_info, index_of, is_five_ghz, is_quiet,
};
pub use even::Even;
pub use weighted::Weighted;

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

/// Deals a set of channels into the Wi-Fi slots' shares.
///
/// [`plan_with`] calls it and holds it to the set: a share is masked to `channels`, and what no
/// share holds is reported unreachable.
pub trait Dealer {
    /// Deal `channels` into `shares`, which holds one empty set per Wi-Fi slot, in slot order,
    /// and is never empty.
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]);
}

/// Which deal the fleet's plan is cut with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Planner {
    /// [`Even`]: shares within one channel of each other.
    #[default]
    Even,
    /// [`Weighted`]: the quiet 5 GHz channels on a few sparse nodes.
    Weighted,
}

impl Planner {
    /// Every planner, in the order the settings modal steps through them.
    pub const ALL: [Self; 2] = [Self::Even, Self::Weighted];
}

impl fmt::Display for Planner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Even => "even",
            Self::Weighted => "weighted",
        })
    }
}

/// How many sparse nodes [`Weighted`] asks for: automatic, or 1 to [`Self::MAX`].
///
/// Serialized as its byte, 0 for automatic, so a byte over [`Self::MAX`] fails to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct SparseNodes(u8);

impl SparseNodes {
    /// One sparse node per five Wi-Fi slots, rounded up.
    pub const AUTO: Self = Self(0);

    /// The most sparse nodes a fixed value asks for.
    pub const MAX: u8 = 4;

    /// Every value, in the order the settings modal steps through them.
    pub const ALL: [Self; 5] = [Self::AUTO, Self(1), Self(2), Self(3), Self(4)];

    /// A fixed count of `n`, or `None` for 0 or over [`Self::MAX`].
    #[must_use]
    pub const fn fixed(n: u8) -> Option<Self> {
        if n == 0 || n > Self::MAX { None } else { Some(Self(n)) }
    }

    /// The value `byte` names, 0 for automatic, or `None` over [`Self::MAX`].
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        if byte > Self::MAX { None } else { Some(Self(byte)) }
    }

    /// The value's byte, 0 for automatic.
    #[must_use]
    pub const fn byte(self) -> u8 {
        self.0
    }

    /// The sparse nodes asked for on `wifi_slots` Wi-Fi slots: `ceil(wifi_slots / 5)` for
    /// automatic, else the fixed count. [`Weighted`] clamps it to what the deal can use.
    #[must_use]
    pub const fn count_for(self, wifi_slots: usize) -> usize {
        match self.0 {
            0 => wifi_slots.div_ceil(5),
            n => n as usize,
        }
    }
}

impl Default for SparseNodes {
    fn default() -> Self {
        Self::AUTO
    }
}

/// A sparse node byte over [`SparseNodes::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseNodesOutOfRange(pub u8);

impl fmt::Display for SparseNodesOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sparse nodes {} is over {}", self.0, SparseNodes::MAX)
    }
}

impl TryFrom<u8> for SparseNodes {
    type Error = SparseNodesOutOfRange;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        Self::from_byte(byte).ok_or(SparseNodesOutOfRange(byte))
    }
}

impl From<SparseNodes> for u8 {
    fn from(sparse: SparseNodes) -> Self {
        sparse.0
    }
}

impl fmt::Display for SparseNodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => f.write_str("auto"),
            n => write!(f, "{n}"),
        }
    }
}

/// The planner the fleet deals with, and the sparse node count [`Weighted`] takes.
///
/// The sparse node count is kept under [`Planner::Even`] too, so switching back to weighted finds
/// it unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSettings {
    /// The deal.
    pub planner: Planner,
    /// The sparse nodes [`Planner::Weighted`] asks for.
    pub sparse_nodes: SparseNodes,
}

impl PlanSettings {
    /// [`Planner::Even`], with [`SparseNodes::AUTO`]: what a bridge with nothing stored deals.
    pub const DEFAULT: Self = Self { planner: Planner::Even, sparse_nodes: SparseNodes::AUTO };
}

impl Default for PlanSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Dealer for PlanSettings {
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]) {
        match self.planner {
            Planner::Even => Even.deal(channels, shares),
            Planner::Weighted => {
                Weighted { sparse_nodes: self.sparse_nodes }.deal(channels, shares)
            }
        }
    }
}

/// What the planner is dealing to, one per fleet slot.
///
/// The fleet planner decides it ([`crate::fleet::pick_bluetooth`]). It reaches [`plan_with`] as
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
    /// Every channel dealt when no slot is sniffing, which happens only when the whole fleet is
    /// the Bluetooth node. Otherwise empty, unless the dealer left a channel out.
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

/// Build a plan distributing `pool` across `node_count` nodes, with the even deal.
///
/// The deal is [`Even`]'s, over the pool's channels: 1, 6 and 11 first, then the rest
/// ascending, so every node gets some of both bands when each band has a channel for every node.
/// Every pool holds more channels than [`MAX_NODES`], so each is dealt once and shares differ by
/// at most one channel.
///
/// Every node is taken to be sniffing Wi-Fi. For a fleet with a Bluetooth node, or another deal,
/// use [`plan_with`].
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    let fleet = [Job::Wifi; MAX_NODES];
    plan_with(pool.channels(), fleet.get(..usize::from(node_count))?, &Even)
}

/// Build a plan distributing `channels` across a fleet of known jobs, with `dealer`'s deal.
///
/// `dealer` is handed one empty share per [`Job::Wifi`] slot, in slot order, and each share it
/// returns goes to that slot, masked to `channels`. A [`Job::Bluetooth`] slot is counted but not
/// dealt to, so the sniffing slots' shares come out as though it were not there.
/// `crates/wartui/README.md` § "Channel pools" is the operator's account.
///
/// [`Plan::unreachable`] is `channels` less every share. When the whole fleet scans Bluetooth the
/// dealer is not called and that is all of `channels`. Such a fleet still gets a plan: a refusal
/// would leave the node on its old share, sniffing Wi-Fi, the opposite of what was asked.
///
/// Returns `None` for an empty fleet, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan_with<D: Dealer + ?Sized>(
    channels: ChannelSet,
    fleet: &[Job],
    dealer: &D,
) -> Option<Plan> {
    let node_count = u8::try_from(fleet.len()).ok()?;
    if node_count == 0 || fleet.len() > MAX_NODES {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let bluetooth =
        fleet.iter().position(|job| matches!(job, Job::Bluetooth)).map(|slot| slot as u8);

    let wifi = || (0..fleet.len()).filter(|node| fleet[*node] == Job::Wifi);
    let mut shares = [ChannelSet::empty(); MAX_NODES];
    let shares = &mut shares[..wifi().count()];
    if !shares.is_empty() {
        dealer.deal(channels, shares);
    }
    let mut slots = [ChannelSet::empty(); MAX_NODES];
    let mut held = 0;
    for (node, share) in wifi().zip(shares.iter()) {
        let share = share.intersection(channels);
        slots[node] = share;
        held |= share.bits();
    }
    let unreachable = ChannelSet::from_bits(channels.bits() & !held);
    Some(Plan { node_count, slots, bluetooth, unreachable })
}
