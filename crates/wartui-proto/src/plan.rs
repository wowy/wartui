//! Channel pools and the assignment planner.
//!
//! A *pool* is still described in runs — [`ChannelPool::Us`] is two of them,
//! with a gap at indices 11-13 — because that is the shape the regulatory
//! picture has. An *assignment* is not. `MSG_ADMIN` carries a forty-bit
//! [`ChannelSet`], one bit per [`SCAN_CHANNELS`] entry, so a node can hold any
//! subset of the pool and a run boundary stops being something the planner has
//! to steer around. Before that it was the central constraint here: a lone node
//! on a two-run pool could not express both runs at once and had to rotate
//! between them on a dwell timer, and every node's share had to be carved out
//! of one run rather than out of the pool.
//!
//! With the mask, the split is a round-robin deal: index `k` of the pool's
//! flattened order goes to node `k % node_count`. Block-splitting would give
//! one node the whole of 2.4 GHz and another the whole of 5 GHz for the same
//! arithmetic, which is the worse partition — dealing gives every node some of
//! both bands, so a node dropping out thins the fleet's coverage evenly rather
//! than blinding it to a band until the next re-cut lands.

use crate::air::AdminMsg;

/// The node's scan order, verbatim from `src/WiFiOps.cpp:53-64`.
///
/// Indices 0..=13 are the 2.4 GHz channels 1..=14; 14..=39 are 5 GHz. Every bit
/// of the [`ChannelSet`] on the wire indexes this table, and a node sweeps in
/// index order, so the order here is load-bearing and must not be sorted or
/// deduplicated — reordering it would silently repoint every assignment in
/// flight and every stored row.
pub const SCAN_CHANNELS: [u8; 40] = [
    // 2.4 GHz
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, //
    // 5 GHz
    36, 40, 44, 48, //
    52, 56, 60, 64, //
    100, 112, 116, 120, 124, 128, 132, 136, 140, 144, //
    149, 153, 157, 161, 165, 169, 173, 177,
];

/// `NUM_SCAN_CHANNELS`, `src/WiFiOps.cpp:66`.
pub const NUM_SCAN_CHANNELS: u8 = 40;

/// The largest fleet wartui supports.
///
/// Twenty, because that is how many peers an ESP-NOW radio can hold and a node
/// this host cannot address is not a node it can drive. The vendor firmware's
/// own table is twenty-four (`src/WiFiOps.h:56`), but the four it has spare are
/// unreachable from here: the bridge would refuse to register them, every
/// assignment to them would be refused, and the planner would be partitioning
/// the channel pool among nodes that never hear the result. Anything above
/// twenty is unsupported rather than degraded.
pub const MAX_NODES: usize = 20;

/// `NODE_STAGGER_WINDOW_MS`, `src/WiFiOps.h:59`.
pub const NODE_STAGGER_WINDOW_MS: u32 = 120;

/// The channel every node returns to in order to speak to the controller.
///
/// `ESPNOW_CHANNEL`, `src/WiFiOps.cpp:15`. Nothing negotiates this: a node that
/// picked a different one would be transmitting into an empty room.
pub const CONTROL_CHANNEL: u8 = 6;

/// How long a node listens on one channel before moving on.
///
/// The vendor's `CHANNEL_TIMER` is 80 ms (`src/configs.h:159`), which is a
/// scan's dwell budget. wartui's node sniffs instead of scanning, so the figure
/// it needs is a beacon interval rather than a probe round-trip: the default
/// interval is 102.4 ms, and anything shorter than that can miss an access
/// point entirely rather than merely hearing it less often.
pub const CHANNEL_DWELL_MS: u32 = 125;

/// How long a node holds the control channel after its heartbeat.
///
/// `ADMIN_WAIT_MS`, `src/WiFiOps.h:58`. This is the window an assignment has to
/// land inside, and the reason the host sends one only in the moment after a
/// heartbeat.
pub const ADMIN_WAIT_MS: u32 = 300;

/// How long an unassigned node waits between heartbeats.
///
/// A node that has been told nothing parks on the control channel and collects
/// nothing, so this is the whole of its cycle rather than a slice of it — which
/// is why it is comfortably longer than [`ADMIN_WAIT_MS`] and still short
/// enough that joining a fleet costs a second rather than a sweep. Shared with
/// the simulator so a parked fake node is parked the same way.
pub const IDLE_BEAT_MS: u32 = 1000;

const _: () = assert!(
    IDLE_BEAT_MS >= ADMIN_WAIT_MS,
    "a parked node must hold the control channel for at least a full admin window"
);

/// How many recently-reported BSSIDs a node suppresses.
///
/// `mac_history_len`, `src/configs.h:158`. Shared between the Wi-Fi and BLE
/// paths, oldest evicted first, and never cleared at runtime — which is why a
/// long-running node's observation stream goes quiet rather than repeating.
pub const DEDUP_RING: usize = 200;

/// Upper bound on runs in any pool. Two today; the headroom is for a
/// "US non-DFS" pool, which would be three.
const MAX_RUNS: usize = 4;

/// An inclusive run of [`SCAN_CHANNELS`] indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRun {
    /// First index, inclusive.
    pub start: u8,
    /// Last index, inclusive.
    pub end: u8,
}

impl IndexRun {
    /// A run covering `start..=end`.
    #[must_use]
    pub const fn new(start: u8, end: u8) -> Self {
        Self { start, end }
    }

    /// How many channels the run covers. Never zero.
    #[must_use]
    pub const fn len(&self) -> u8 {
        self.end - self.start + 1
    }

    /// Always false; a run is inclusive and so covers at least one channel.
    /// Present because clippy asks for it alongside [`Self::len`].
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Whether `idx` falls inside the run.
    #[must_use]
    pub const fn contains(&self, idx: u8) -> bool {
        idx >= self.start && idx <= self.end
    }
}

/// A set of [`SCAN_CHANNELS`] indices — the forty bits `MSG_ADMIN` carries.
///
/// One bit per entry of the table, index `i` in bit `i`, so the set is exactly
/// as expressive as the wire field and a node can be given any subset of the
/// pool. That is the whole of what Phase 2 changed: an assignment used to be a
/// pair of bounds, which could not describe the US pool's two runs at once and
/// so made a lone node rotate between them.
///
/// Bits at or above [`NUM_SCAN_CHANNELS`] are not representable and are dropped
/// on the way in rather than rejected. A frame carrying one came from something
/// that knows about channels this build does not, and the indices it *does*
/// share are still the right ones to scan; refusing the whole assignment would
/// strand the node on whatever it held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ChannelSet(u64);

/// Bytes a [`ChannelSet`] occupies on the wire. Forty bits, little-endian.
pub const CHANNEL_SET_BYTES: usize = 5;

const CHANNEL_SET_MASK: u64 = (1u64 << NUM_SCAN_CHANNELS) - 1;

impl ChannelSet {
    /// The empty set. A node is never *sent* one: there is no frame meaning
    /// "scan nothing", so the planner skips a node it has nothing for and the
    /// engine ignores a hand-assignment of one.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Every index in `run`.
    #[must_use]
    pub const fn from_run(run: IndexRun) -> Self {
        // `run.len()` is at least 1, so the shift is at most 40 and the
        // subtraction cannot underflow.
        let width = run.len() as u32;
        let bits = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
        Self((bits << run.start) & CHANNEL_SET_MASK)
    }

    /// The raw bits, index `i` in bit `i`.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// A set from raw bits, discarding anything this build cannot scan.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits & CHANNEL_SET_MASK)
    }

    /// Add one index. Out of range is a no-op, for the reason on the type.
    pub const fn insert(&mut self, idx: u8) {
        if idx < NUM_SCAN_CHANNELS {
            self.0 |= 1u64 << idx;
        }
    }

    /// Whether the set holds `idx`.
    #[must_use]
    pub const fn contains(self, idx: u8) -> bool {
        idx < NUM_SCAN_CHANNELS && (self.0 >> idx) & 1 == 1
    }

    /// How many channels the node holding this would dwell on per sweep.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// Whether there is nothing to scan.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The indices, ascending, which is [`SCAN_CHANNELS`] order and therefore
    /// the order a node sweeps them in.
    #[must_use]
    pub const fn indices(self) -> ChannelSetIter {
        ChannelSetIter(self.0)
    }

    /// The lowest index in the set, if any.
    #[must_use]
    pub const fn first(self) -> Option<u8> {
        if self.0 == 0 {
            return None;
        }
        // At most 39: the mask keeps every set bit below NUM_SCAN_CHANNELS.
        #[allow(clippy::cast_possible_truncation)]
        Some(self.0.trailing_zeros() as u8)
    }

    /// The five wire bytes, little-endian.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; CHANNEL_SET_BYTES] {
        let all = self.0.to_le_bytes();
        [all[0], all[1], all[2], all[3], all[4]]
    }

    /// A set from the five wire bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; CHANNEL_SET_BYTES]) -> Self {
        Self::from_bits(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], 0, 0, 0,
        ]))
    }
}

/// The indices of a [`ChannelSet`], ascending.
#[derive(Debug, Clone, Copy)]
pub struct ChannelSetIter(u64);

impl Iterator for ChannelSetIter {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.0 == 0 {
            return None;
        }
        // Below NUM_SCAN_CHANNELS by construction, so the cast is lossless.
        #[allow(clippy::cast_possible_truncation)]
        let idx = self.0.trailing_zeros() as u8;
        // Clear the lowest set bit.
        self.0 &= self.0 - 1;
        Some(idx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.0.count_ones() as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for ChannelSetIter {}

const US_RUNS: [IndexRun; 2] = [
    // 2.4 GHz channels 1-11. Excludes 12, 13 and 14.
    IndexRun::new(0, 10),
    // 5 GHz channels 36-165. Excludes the UNII-4 channels 169, 173 and 177.
    IndexRun::new(14, 36),
];

const ALL_RUNS: [IndexRun; 2] = [
    // Everything below channel 14.
    IndexRun::new(0, 12),
    // Everything above it, to the end of the table.
    IndexRun::new(14, NUM_SCAN_CHANNELS - 1),
];

/// Channel 14, which no pool contains and no node can tune.
///
/// `esp-radio` hardcodes `schan: 1, nchan: 13` in the country blob and exposes
/// neither, so a node handed this index refuses the hop — once per sweep, every
/// sweep, for the life of the assignment. Reaching the field needs
/// `esp_wifi_set_country` called directly, which means `unsafe` in a crate that
/// forbids it. `docs/phase-1-findings.md` has the measurement and the reading of
/// the driver.
///
/// So it is unsupported rather than merely unused, and it is excluded here
/// rather than left for the operator to avoid: a pool that contains a channel
/// the fleet cannot tune spends a dwell of every sweep on nothing and reports
/// the refusal only to a serial console nobody is watching. It stays in
/// [`SCAN_CHANNELS`] because that table's indices are the wire format and
/// removing an entry would repoint every assignment in flight and every stored
/// row.
pub const UNSUPPORTED_INDEX: u8 = 13;

const _: () = assert!(
    SCAN_CHANNELS[UNSUPPORTED_INDEX as usize] == 14,
    "UNSUPPORTED_INDEX must still be channel 14"
);

// `MAX_RUNS` bounds nothing the planner indexes any more — it deals out of a
// flattened iterator — but it still records what a pool is allowed to look
// like, and a pool exceeding it is a pool nobody thought about. A new one must
// be added here as well as to `ChannelPool::runs`.
const _: () = assert!(
    US_RUNS.len() <= MAX_RUNS && ALL_RUNS.len() <= MAX_RUNS,
    "a channel pool has more runs than MAX_RUNS; raise it"
);

/// Which channels the fleet is allowed to scan.
///
/// For a wartui node this bounds where the radio *listens*: it parks and reads
/// beacons rather than probing, so a restricted pool is a choice about coverage
/// rather than about legality.
///
/// For a stock node it bounds where the node *transmits*. Every
/// `WiFi.scanNetworks` call in the vendor firmware passes `passive = false`
/// (`src/WiFiOps.cpp:745,755,779,3311`), so it sends a probe request on each
/// channel it is assigned — and the host cannot change that from here, which is
/// why the pool exists at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelPool {
    /// FCC-permitted unlicensed WLAN channels: 2.4 GHz 1-11 and 5 GHz 36-165.
    ///
    /// Note that 5 GHz 52-144 are DFS channels, where the rules require passive
    /// scanning. A wartui node always listens and so is welcome there; a stock
    /// node always scans actively, and the host cannot change that.
    #[default]
    Us,
    /// Every channel a node can actually tune: 2.4 GHz 1-13 and all of 5 GHz.
    ///
    /// The unrestricted pool, and the one to put a fleet on to sweep as widely
    /// as the hardware allows — including the UNII-4 channels 169, 173 and 177
    /// that [`Self::Us`] leaves out.
    ///
    /// It is 39 channels and not 40. Channel 14 is unsupported: see
    /// [`UNSUPPORTED_INDEX`]. That makes this pool two runs rather than one,
    /// which before the channel mask would have cost a lone node a rotation and
    /// is now one clear bit.
    All,
}

impl ChannelPool {
    /// The contiguous runs making up this pool, in ascending index order.
    #[must_use]
    pub const fn runs(self) -> &'static [IndexRun] {
        match self {
            Self::Us => &US_RUNS,
            Self::All => &ALL_RUNS,
        }
    }

    /// Total channels in the pool.
    #[must_use]
    pub fn channel_count(self) -> u16 {
        self.runs().iter().map(|r| u16::from(r.len())).sum()
    }

    /// Whether the pool includes a given index.
    #[must_use]
    pub fn contains(self, idx: u8) -> bool {
        self.runs().iter().any(|r| r.contains(idx))
    }

    /// The whole pool as one set — every channel a single node could be told
    /// to hold, which before the mask was not something one assignment could
    /// say.
    #[must_use]
    pub fn channels(self) -> ChannelSet {
        self.runs().iter().fold(ChannelSet::empty(), |set, run| {
            ChannelSet::from_bits(set.bits() | ChannelSet::from_run(*run).bits())
        })
    }
}

/// How the pool is named on screen and in prose — "US", not the variant's `Us`.
///
/// This is a label, not an identifier: `wartui-core`'s `pool_name` keeps its own
/// lowercase spelling for the `session.channel_pool` column, which has stored
/// rows behind it and must not follow this.
impl core::fmt::Display for ChannelPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Us => "US",
            Self::All => "All",
        })
    }
}

/// A fleet-wide assignment: one [`ChannelSet`] per node.
///
/// There is exactly one of these per fleet membership. It has no phases and no
/// timer behind it, because a mask can say everything a node needs to hold —
/// which is the difference Phase 2 made. The plan before it could not give a
/// lone node both of the US pool's runs at once, so it described a *rotation*
/// and the caller had to step through it, re-issuing `MSG_ADMIN` on a dwell
/// timer and accepting that coverage was intermittent in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    node_count: u8,
    slots: [ChannelSet; MAX_NODES],
}

impl Plan {
    /// Number of nodes the plan was built for.
    #[must_use]
    pub const fn node_count(&self) -> u8 {
        self.node_count
    }

    /// What `node_index` should scan, if anything.
    ///
    /// `None` for an index outside the fleet, and for the empty set — which is
    /// reachable only with more nodes than the pool has channels. There is no
    /// frame meaning "scan nothing", so a caller that gets `None` leaves the
    /// node holding whatever it already has rather than inventing one.
    #[must_use]
    pub fn channels_for(&self, node_index: u8) -> Option<ChannelSet> {
        let set = *self.slots.get(usize::from(node_index))?;
        (node_index < self.node_count && !set.is_empty()).then_some(set)
    }

    /// The `MSG_ADMIN` to send to `node_index`.
    ///
    /// `assignment_version` is the caller's persisted epoch byte; a node adopts
    /// the assignment only when it differs from the one it holds. `flags` is
    /// the caller's, not the planner's: which node scans Bluetooth is an
    /// operator's decision about one node, and partitioning channels is a
    /// decision about the fleet.
    #[must_use]
    pub fn admin_for(&self, node_index: u8, assignment_version: u8, flags: u8) -> Option<AdminMsg> {
        Some(AdminMsg {
            assignment_version,
            node_index,
            node_count: self.node_count,
            flags,
            channels: self.channels_for(node_index)?,
        })
    }
}

/// Build a plan distributing `pool` across `node_count` nodes.
///
/// The pool's runs are flattened into one ascending list of indices and dealt
/// round-robin: index `k` goes to node `k % node_count`. Every node therefore
/// gets a share of every run — some 2.4 GHz and some 5 GHz on the US pool —
/// rather than a block, which for the same arithmetic would have put one node
/// on 2.4 GHz alone and left the fleet blind to a whole band the moment that
/// node dropped out.
///
/// Shares differ by at most one channel, so heartbeat periods across the fleet
/// stay within one dwell of each other. `node_index` is a single fleet-wide
/// `0..node_count` numbering because it drives the transmit stagger slot
/// ([`stagger_offset_ms`]), which has to be unique across the whole fleet.
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    if node_count == 0 || usize::from(node_count) > MAX_NODES {
        return None;
    }
    let mut slots = [ChannelSet::empty(); MAX_NODES];
    let mut dealt = 0usize;
    for run in pool.runs() {
        for idx in run.start..=run.end {
            slots[dealt % usize::from(node_count)].insert(idx);
            dealt += 1;
        }
    }
    Some(Plan { node_count, slots })
}

/// How long node `node_index` waits before transmitting its heartbeat.
///
/// Verbatim port of `calculateNodeStaggerOffsetMs`, `src/RadioTuning.cpp:3-13`.
/// A lone node owns the channel, and an index outside the assignment means the
/// core has not placed the node yet; neither should delay anything.
#[must_use]
pub const fn stagger_offset_ms(node_index: u8, node_count: u8, window_ms: u32) -> u32 {
    if node_count <= 1 || node_index >= node_count {
        return 0;
    }
    (node_index as u32 * window_ms) / node_count as u32
}
