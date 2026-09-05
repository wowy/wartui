//! Channel pools and the assignment planner.
//!
//! `MSG_ADMIN` can express only a single *contiguous* run of indices into
//! [`SCAN_CHANNELS`]. That is fine for the stock firmware, which always splits
//! all 40 entries, but a restricted pool such as [`ChannelPool::Us`] is two
//! runs with a gap in the middle. So the planner works in runs and never lets a
//! node's range straddle one.

use crate::air::AdminMsg;

/// The node's scan order, verbatim from `src/WiFiOps.cpp:53-64`.
///
/// Indices 0..=13 are the 2.4 GHz channels 1..=14; 14..=39 are 5 GHz. Every
/// `start_channel_idx` / `end_channel_idx` on the wire indexes this table, so
/// its order is load-bearing and must not be sorted or deduplicated.
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

/// `MAX_NODES`, `src/WiFiOps.h:56`. The firmware's node table is this big, so
/// there is no point planning for more.
pub const MAX_NODES: usize = 24;

/// `NODE_STAGGER_WINDOW_MS`, `src/WiFiOps.h:59`.
pub const NODE_STAGGER_WINDOW_MS: u32 = 120;

/// Upper bound on runs in any pool. Two today; the headroom is for a
/// "US non-DFS" pool, which would be three.
const MAX_RUNS: usize = 4;

/// Upper bound on rotation phases, reached when one node must cover every run.
const MAX_PHASES: usize = MAX_RUNS;

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

const US_RUNS: [IndexRun; 2] = [
    // 2.4 GHz channels 1-11. Excludes 12, 13 and 14.
    IndexRun::new(0, 10),
    // 5 GHz channels 36-165. Excludes the UNII-4 channels 169, 173 and 177.
    IndexRun::new(14, 36),
];

const ALL_RUNS: [IndexRun; 1] = [IndexRun::new(0, NUM_SCAN_CHANNELS - 1)];

/// Which channels the fleet is allowed to scan.
///
/// This constrains where nodes *transmit*, not merely where they listen: every
/// `WiFi.scanNetworks` call in the firmware passes `passive = false`
/// (`src/WiFiOps.cpp:745,755,779,3311`), so a node sends probe requests on each
/// channel it is assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelPool {
    /// FCC-permitted unlicensed WLAN channels: 2.4 GHz 1-11 and 5 GHz 36-165.
    ///
    /// Note that 5 GHz 52-144 are DFS channels, where the rules require passive
    /// scanning; the node firmware always scans actively and we cannot change
    /// that from the host.
    #[default]
    Us,
    /// Every channel the firmware knows, matching stock node behaviour.
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
}

/// A fleet-wide assignment, possibly rotating.
///
/// When there are at least as many nodes as runs, there is a single phase and
/// every node holds a fixed range. When nodes are scarcer than runs — reachable
/// only with one node on a multi-run pool — the plan has several phases and the
/// caller steps through them on a dwell timer, re-issuing `MSG_ADMIN` each time.
/// Coverage then becomes intermittent rather than incorrect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    node_count: u8,
    phase_count: u8,
    slots: [[Option<IndexRun>; MAX_NODES]; MAX_PHASES],
}

impl Plan {
    /// Number of nodes the plan was built for.
    #[must_use]
    pub const fn node_count(&self) -> u8 {
        self.node_count
    }

    /// How many phases a full rotation takes. 1 means no rotation.
    #[must_use]
    pub const fn phase_count(&self) -> u8 {
        self.phase_count
    }

    /// Whether the plan rotates, so callers know to run a dwell timer.
    #[must_use]
    pub const fn rotates(&self) -> bool {
        self.phase_count > 1
    }

    /// The range assigned to `node_index` during `phase`, if any.
    #[must_use]
    pub fn range_for(&self, node_index: u8, phase: u8) -> Option<IndexRun> {
        let phase = usize::from(phase % self.phase_count.max(1));
        self.slots.get(phase)?.get(usize::from(node_index)).copied().flatten()
    }

    /// The `MSG_ADMIN` to send to `node_index` for `phase`.
    ///
    /// `assignment_version` is the caller's persisted epoch byte; a node adopts
    /// the assignment only when it differs from the one it holds.
    #[must_use]
    pub fn admin_for(&self, node_index: u8, phase: u8, assignment_version: u8) -> Option<AdminMsg> {
        let run = self.range_for(node_index, phase)?;
        Some(AdminMsg {
            assignment_version,
            node_index,
            node_count: self.node_count,
            start_channel_idx: run.start,
            end_channel_idx: run.end,
        })
    }
}

/// Build a plan distributing `pool` across `node_count` nodes.
///
/// Nodes are apportioned to runs in proportion to run length, every run getting
/// at least one, and each run is then subdivided with the same integer
/// arithmetic the firmware uses (`src/WiFiOps.cpp:507-508`). `node_index` stays
/// a single fleet-wide `0..node_count` numbering rather than restarting per
/// run, because it drives the transmit stagger slot
/// ([`stagger_offset_ms`]) which has to stay unique across the whole fleet.
///
/// Returns `None` for zero nodes, or for more than [`MAX_NODES`].
#[must_use]
pub fn plan(pool: ChannelPool, node_count: u8) -> Option<Plan> {
    if node_count == 0 || usize::from(node_count) > MAX_NODES {
        return None;
    }
    let runs = pool.runs();
    let mut slots = [[None; MAX_NODES]; MAX_PHASES];

    if usize::from(node_count) < runs.len() {
        // Too few nodes to hold every run at once: rotate whole runs.
        let phase_count = runs.len().div_ceil(usize::from(node_count));
        for (phase, slot_row) in slots.iter_mut().enumerate().take(phase_count) {
            let base = phase * usize::from(node_count);
            for (node, slot) in slot_row.iter_mut().enumerate().take(usize::from(node_count)) {
                // Runs run out on the last phase when the count does not divide
                // evenly; those nodes simply idle for that phase.
                *slot = runs.get(base + node).copied();
            }
        }
        // Cast is safe: phase_count <= runs.len() <= MAX_RUNS.
        #[allow(clippy::cast_possible_truncation)]
        return Some(Plan { node_count, phase_count: phase_count as u8, slots });
    }

    let alloc = apportion(runs, node_count);
    let mut next_index = 0usize;
    for (run_idx, run) in runs.iter().enumerate() {
        let k = alloc[run_idx];
        for n in 0..k {
            slots[0][next_index] = Some(subdivide(*run, n, k));
            next_index += 1;
        }
    }
    Some(Plan { node_count, phase_count: 1, slots })
}

/// Spread `node_count` nodes over `runs` proportionally to run length, giving
/// every run at least one node. Largest-remainder apportionment.
fn apportion(runs: &[IndexRun], node_count: u8) -> [u8; MAX_RUNS] {
    let mut alloc = [0u8; MAX_RUNS];
    for slot in alloc.iter_mut().take(runs.len()) {
        *slot = 1;
    }
    // `plan` guarantees node_count >= runs.len(), so this cannot underflow.
    let mut spare = u32::from(node_count) - runs.len() as u32;
    if spare == 0 {
        return alloc;
    }

    let total: u32 = runs.iter().map(|r| u32::from(r.len())).sum();
    // Snapshot the base: `spare` is drawn down as shares are handed out, and
    // weighing a later run against the reduced figure would skew the split.
    let base = spare;
    let mut remainders = [(0u32, 0usize); MAX_RUNS];
    for (i, run) in runs.iter().enumerate() {
        let weighted = base * u32::from(run.len());
        // Cast is safe: the floor share is at most `spare`, itself < MAX_NODES.
        #[allow(clippy::cast_possible_truncation)]
        let share = (weighted / total) as u8;
        alloc[i] += share;
        spare -= u32::from(share);
        remainders[i] = (weighted % total, i);
    }

    // Hand out what integer division left over, biggest remainder first,
    // breaking ties towards the earlier run so the result is deterministic.
    remainders[..runs.len()].sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for &(_, run_idx) in remainders[..runs.len()].iter().cycle().take(spare as usize) {
        alloc[run_idx] += 1;
    }
    alloc
}

/// Carve the `n`th of `k` slices out of `run`, mirroring the firmware's split.
///
/// When nodes outnumber the run's channels the vendor arithmetic would compute
/// an end before the start and underflow its `uint8_t`; we clamp to a single
/// channel instead, so surplus nodes overlap rather than idle.
fn subdivide(run: IndexRun, n: u8, k: u8) -> IndexRun {
    let len = u16::from(run.len());
    let (n, k) = (u16::from(n), u16::from(k));
    // Cast is safe: both quotients are < len <= NUM_SCAN_CHANNELS.
    #[allow(clippy::cast_possible_truncation)]
    let start = ((n * len) / k) as u8;
    #[allow(clippy::cast_possible_truncation)]
    let end = (((n + 1) * len) / k).saturating_sub(1) as u8;
    IndexRun::new(run.start + start, run.start + end.max(start))
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
