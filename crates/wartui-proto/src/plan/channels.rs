use super::Radio;

/// The node's scan order.
///
/// Indices 0..=13 are the 2.4 GHz channels 1..=14; 14..=41 are 5 GHz. Every bit of
/// the [`ChannelSet`] on the wire indexes this table, so the order must never be
/// sorted or deduplicated: reordering it silently repoints every assignment in
/// flight and every stored row.
pub const SCAN_CHANNELS: [u8; 42] = [
    // 2.4 GHz
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, //
    // 5 GHz
    36, 40, 44, 48, //
    52, 56, 60, 64, //
    100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, //
    149, 153, 157, 161, 165, 169, 173, 177,
];

/// How many entries [`SCAN_CHANNELS`] has.
pub const NUM_SCAN_CHANNELS: u8 = 42;

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

/// A set of [`SCAN_CHANNELS`] indices — the forty-two bits an assignment carries.
///
/// One bit per entry of the table, index `i` in bit `i`, so the set is exactly as
/// expressive as the wire field.
///
/// Bits at or above [`NUM_SCAN_CHANNELS`] are dropped on the way in rather than
/// rejected: such a frame came from a build that knows channels this one does
/// not, the indices it *does* share are still right, and refusing the whole
/// assignment would strand the node on whatever it held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ChannelSet(u64);

/// Bytes a [`ChannelSet`] occupies on the wire. Forty-two bits, little-endian.
pub const CHANNEL_SET_BYTES: usize = 6;

const CHANNEL_SET_MASK: u64 = (1u64 << NUM_SCAN_CHANNELS) - 1;

impl ChannelSet {
    /// The empty set.
    ///
    /// Sent only alongside [`crate::air::ADMIN_FLAG_BLE`], where it does not mean
    /// "scan nothing" but "Bluetooth is the whole of the job" — see
    /// [`Job::Bluetooth`]. Without the flag there is no frame meaning "scan
    /// nothing", so the planner skips a node it has nothing for rather than
    /// telling it to stop: a node sent one would park.
    ///
    /// [`Job::Bluetooth`]: super::Job::Bluetooth
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Every index in `run`.
    #[must_use]
    pub const fn from_run(run: IndexRun) -> Self {
        // `run.len()` is at most `NUM_SCAN_CHANNELS` and at least 1, so the
        // shift stays under 64 and the subtraction cannot underflow.
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
        // At most 41: the mask keeps every set bit below NUM_SCAN_CHANNELS.
        #[allow(clippy::cast_possible_truncation)]
        Some(self.0.trailing_zeros() as u8)
    }

    /// The six wire bytes, little-endian.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; CHANNEL_SET_BYTES] {
        let all = self.0.to_le_bytes();
        [all[0], all[1], all[2], all[3], all[4], all[5]]
    }

    /// A set from the six wire bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; CHANNEL_SET_BYTES]) -> Self {
        Self::from_bits(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], 0, 0,
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
    IndexRun::new(14, 38),
];

const EU_RUNS: [IndexRun; 2] = [
    // 2.4 GHz channels 1-13. Excludes 14, which is in no pool.
    IndexRun::new(0, 12),
    // 5 GHz 36-140: 5150-5350 and 5470-5725. Excludes 144, whose 20 MHz slot
    // runs past 5725, and all of 149-177.
    IndexRun::new(14, 32),
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
/// neither, so a node handed this index refuses the hop once per sweep, silently,
/// for the life of the assignment (`docs/phase-1-findings.md`). Reaching the field
/// needs `esp_wifi_set_country` called directly, and so `unsafe`.
///
/// Unsupported rather than merely unused, and so excluded from every pool. It
/// stays in [`SCAN_CHANNELS`] because that table's indices are the wire format.
pub const UNSUPPORTED_INDEX: u8 = 13;

const _: () = assert!(
    SCAN_CHANNELS[UNSUPPORTED_INDEX as usize] == 14,
    "UNSUPPORTED_INDEX must still be channel 14"
);

/// The first [`SCAN_CHANNELS`] entry that needs a 5 GHz radio.
///
/// The table is 2.4 GHz then 5 GHz, so band membership is a comparison rather than
/// a lookup — asserted against the table rather than written down as 14.
pub const FIRST_FIVE_GHZ_INDEX: u8 = 14;

const _: () = assert!(
    SCAN_CHANNELS[FIRST_FIVE_GHZ_INDEX as usize - 1] == 14
        && SCAN_CHANNELS[FIRST_FIVE_GHZ_INDEX as usize] == 36,
    "SCAN_CHANNELS is no longer 2.4 GHz followed by 5 GHz"
);

/// Whether tuning `idx` needs a 5 GHz radio.
#[must_use]
pub const fn is_five_ghz(idx: u8) -> bool {
    idx >= FIRST_FIVE_GHZ_INDEX
}

// The planner deals out of a flattened iterator and indexes nothing by run, but
// a pool exceeding `MAX_RUNS` is a pool nobody thought about. A new one goes here
// as well as in `ChannelPool::runs`.
const _: () = assert!(
    US_RUNS.len() <= MAX_RUNS && EU_RUNS.len() <= MAX_RUNS && ALL_RUNS.len() <= MAX_RUNS,
    "a channel pool has more runs than MAX_RUNS; raise it"
);

/// Which channels the fleet is allowed to scan.
///
/// For a wartui node this bounds where the radio *listens*: it parks and reads
/// beacons rather than probing, so a restricted pool is a choice about coverage
/// rather than about legality.
///
/// A pool cannot make a node quiet; only the node's own firmware can, which is why
/// that half of the project is in `firmware/node` rather than here. See
/// [`crate::beacon`] for why an active scan does not belong on DFS channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelPool {
    /// FCC-permitted unlicensed WLAN channels: 2.4 GHz 1-11 and 5 GHz 36-165.
    ///
    /// 5 GHz 52-144 are DFS channels, where the rules require passive scanning. A
    /// wartui node always listens and so is welcome there.
    Us,
    /// ETSI-permitted unlicensed WLAN channels: 2.4 GHz 1-13 and 5 GHz 36-140.
    ///
    /// The 5 GHz half is 5150-5350 and 5470-5725, ending at channel 140: 144's
    /// twenty megahertz run past 5725, and 149 upwards is another band again.
    /// The 2.4 GHz half is wider than [`Self::Us`] by channels 12 and 13.
    Eu,
    /// Every channel a node can actually tune: 2.4 GHz 1-13 and all of 5 GHz.
    ///
    /// The unrestricted pool, including the UNII-4 channels 169, 173 and 177 that
    /// [`Self::Us`] leaves out. 41 channels and not 42, and two runs rather than
    /// one, because channel 14 is unsupported — see [`UNSUPPORTED_INDEX`].
    ///
    /// The default. A pool bounds where a node listens rather than what it emits,
    /// so narrowing it to one regulatory domain is a choice about coverage that
    /// an operator in another domain pays for in channels never swept.
    #[default]
    All,
}

impl ChannelPool {
    /// The contiguous runs making up this pool, in ascending index order.
    #[must_use]
    pub const fn runs(self) -> &'static [IndexRun] {
        match self {
            Self::Us => &US_RUNS,
            Self::Eu => &EU_RUNS,
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

    /// The whole pool as one set: every channel a single node could be told to hold.
    #[must_use]
    pub fn channels(self) -> ChannelSet {
        self.runs().iter().fold(ChannelSet::empty(), |set, run| {
            ChannelSet::from_bits(set.bits() | ChannelSet::from_run(*run).bits())
        })
    }

    /// Every channel of the pool that `radio` can tune.
    ///
    /// Never empty: every pool has 2.4 GHz in it and every [`Radio`] reaches 2.4 GHz.
    /// That is what makes this a usable answer for a node the deal had nothing for —
    /// see [`Plan::channels_for`].
    ///
    /// [`Plan::channels_for`]: super::Plan::channels_for
    #[must_use]
    pub fn reachable_by(self, radio: Radio) -> ChannelSet {
        let mut set = ChannelSet::empty();
        for idx in self.channels().indices().filter(|idx| radio.can_tune(*idx)) {
            set.insert(idx);
        }
        set
    }
}

/// How the pool is named on screen and in prose — "US", not the variant's `Us`.
///
/// This is a label, not an identifier: `wartui-core`'s `pool_name` keeps its own
/// lowercase spelling for the `capture.channel_pool` column, which has stored
/// rows behind it and must not follow this.
impl core::fmt::Display for ChannelPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Us => "US",
            Self::Eu => "EU",
            Self::All => "All",
        })
    }
}
