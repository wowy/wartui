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
//! Bluetooth and that node is dealt nothing. It is still a slot: `node_index` and
//! `node_count` are the fleet's transmit-stagger arithmetic ([`stagger_offset_ms`])
//! rather than a census of who is sniffing, so cutting it out of the deal must not
//! cut it out of the numbering.

use crate::air::AdminMsg;

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

/// The largest fleet wartui supports.
///
/// Twenty, because that is how many peers an ESP-NOW radio can hold, and a node
/// this host cannot address is not one it can drive. Anything above twenty is
/// unsupported rather than degraded.
pub const MAX_NODES: usize = 20;

/// The window a fleet's heartbeat transmissions are staggered across.
pub const NODE_STAGGER_WINDOW_MS: u32 = 120;

/// The channel every node returns to in order to speak to the controller.
///
/// Nothing negotiates this: a node that
/// picked a different one would be transmitting into an empty room.
pub const CONTROL_CHANNEL: u8 = 6;

/// The transmit power every bridge and node is capped at, in `esp-radio`'s quarter-dBm
/// units: 8 is 2 dBm, the lowest `set_max_tx_power` accepts.
///
/// The operator's policy for the whole fleet, not a per-board tuning knob. A fleet rides
/// in one vehicle with its bridge, so the 20 dBm default buys range nothing here needs
/// and puts a node beside its bridge far above the level a receiver is designed for: on
/// the bench a C6 at the default arrived at −21 dBm from a few centimetres away. The cost
/// is range, and it is paid first by heartbeats from a node carried farther off.
pub const TX_POWER_QUARTER_DBM: i8 = 8;

const _: () = assert!(
    TX_POWER_QUARTER_DBM >= 8 && TX_POWER_QUARTER_DBM <= 84,
    "esp-radio's set_max_tx_power accepts 8 to 84 quarter-dBm"
);

/// How long a node listens on one channel before moving on.
///
/// A sniffing node needs a beacon interval rather than a scan's dwell budget: the
/// default interval is 102.4 ms, and anything shorter can miss an access point entirely.
pub const CHANNEL_DWELL_MS: u32 = 125;

/// How long a node holds the control channel after its heartbeat.
///
/// This is the window an assignment has to
/// land inside, and the reason the host sends one only in the moment after a
/// heartbeat. It is also, once per sweep, time in which no channel is swept.
///
/// 100 ms is about five times the slowest the host answered across an hour of
/// captured traffic, and a window missed costs one sweep rather than the
/// assignment, because the node stays dirty and its next heartbeat re-sends
/// (`docs/phase-4-findings.md`, "The admin window, from a captured hour").
pub const ADMIN_WAIT_MS: u32 = 100;

/// How long an unassigned node waits between heartbeats.
///
/// A node told nothing parks on the control channel, so this is the whole of its
/// cycle rather than a slice: longer than [`ADMIN_WAIT_MS`], and short enough that
/// joining a fleet costs a second rather than a sweep.
pub const IDLE_BEAT_MS: u32 = 1000;

const _: () = assert!(
    IDLE_BEAT_MS >= ADMIN_WAIT_MS,
    "a parked node must hold the control channel for at least a full admin window"
);

/// How often the node scanning Bluetooth starts a scan.
///
/// Scan start to scan start, so a scan that overran in a busy room is followed
/// immediately by the next one rather than pushing the cadence out. Such a node
/// sweeps no channels ([`Job::Bluetooth`]), so this is the whole of its cycle and
/// therefore the period its heartbeats arrive at.
///
/// Deliberately its own number rather than [`IDLE_BEAT_MS`], which it happens to
/// equal: one is how long joining a fleet takes and the other a working node's duty
/// cycle, and spelling the coincidence as a rule would make a change to either wrong.
pub const BLE_BEAT_MS: u32 = 1000;

const _: () = assert!(
    BLE_BEAT_MS >= ADMIN_WAIT_MS,
    "a Bluetooth node must hold the control channel for at least a full admin window"
);

/// How many recently-reported addresses a node holds. See [`crate::dedup`].
pub const DEDUP_RING: usize = 200;

/// How long a node suppresses an address it has reported before reporting it again.
///
/// Five minutes: long enough that a stationary node's whole neighbourhood costs a few
/// sightings a minute, short enough that a fresh host session hears it without a reboot.
pub const DEDUP_REFRESH_MS: u32 = 5 * 60 * 1000;

/// How much stronger a sighting must be than any reported for its address to be
/// reported again before the refresh.
pub const DEDUP_RSSI_GAIN_DB: i8 = 10;

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

/// Where a node is in its sweep of the set it was assigned.
///
/// Arithmetic rather than radio, so it lives here and is checked by
/// `cargo test` rather than by a reflash. The thing it exists to get right is
/// the seam between adopting an assignment and dwelling on it: a node steps
/// this at the foot of every pass, *after* the dwell and the report, so a
/// cursor that started life sitting on the lowest index would be stepped past
/// before that channel was ever listened to. The lowest channel of every fresh
/// assignment would then go uncollected until the sweep came round — a hole
/// that reports as nothing at all, on the one channel most likely to be busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepCursor {
    at: Option<u8>,
}

impl SweepCursor {
    /// A cursor that has not begun, which is not the same as one on the first
    /// index. This is the state to put it in when an assignment is adopted.
    #[must_use]
    pub const fn new() -> Self {
        Self { at: None }
    }

    /// The [`SCAN_CHANNELS`] index being dwelt on, or `None` before the first
    /// step of a sweep.
    #[must_use]
    pub const fn index(self) -> Option<u8> {
        self.at
    }

    /// Step to the next index of `channels`, in ascending order, saying whether
    /// that wrapped — which is what a node counts as a completed sweep and
    /// answers with a heartbeat.
    ///
    /// The set is passed in rather than held because the assignment owns it: one
    /// copy means the two cannot disagree about which indices exist.
    pub fn advance(&mut self, channels: ChannelSet) -> bool {
        let next = match self.at {
            Some(at) => channels.indices().find(|idx| *idx > at),
            None => channels.first(),
        };
        match next {
            Some(idx) => {
                self.at = Some(idx);
                false
            }
            None => {
                self.at = channels.first();
                true
            }
        }
    }
}

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
    /// Still a slot: it keeps its `node_index` and counts in `node_count`, because
    /// those are the fleet's transmit-stagger arithmetic rather than a description
    /// of who is sniffing. The shares around it are cut as though it were present
    /// and idle.
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
            Self::Eu => "EU",
            Self::All => "All",
        })
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

    /// What `node_index` should scan, if anything.
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
    ///   the node holding whatever it already has rather than inventing one.
    #[must_use]
    pub fn channels_for(&self, node_index: u8) -> Option<ChannelSet> {
        let set = *self.slots.get(usize::from(node_index))?;
        if node_index >= self.node_count {
            return None;
        }
        if self.bluetooth == Some(node_index) {
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

    /// The assignment to send to `node_index`.
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
    pub fn admin_for(&self, node_index: u8, epoch: u8, flags: u8) -> Option<AdminMsg> {
        let channels = self.channels_for(node_index)?;
        if channels.is_empty() && flags & crate::air::ADMIN_FLAG_BLE == 0 {
            return None;
        }
        Some(AdminMsg { epoch, node_index, node_count: self.node_count, flags, channels })
    }
}

/// Build a plan distributing `pool` across `node_count` nodes.
///
/// The pool's runs are flattened into one ascending list and dealt round-robin:
/// index `k` goes to node `k % node_count`, so every node gets some of every run.
/// Shares differ by at most one channel, so heartbeat periods stay within one
/// dwell of each other. `node_index` is a single fleet-wide `0..node_count`
/// numbering because it drives the transmit stagger ([`stagger_offset_ms`]).
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
/// - **A Bluetooth slot is counted but not dealt to.** `node_count` includes it and
///   it keeps its index, because those are the stagger's arithmetic; see
///   [`Job::Bluetooth`].
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

/// How long node `node_index` waits before transmitting its heartbeat.
///
/// A lone node owns the channel, and an index outside the assignment means the
/// core has not placed the node yet; neither should delay anything.
#[must_use]
pub const fn stagger_offset_ms(node_index: u8, node_count: u8, window_ms: u32) -> u32 {
    if node_count <= 1 || node_index >= node_count {
        return 0;
    }
    (node_index as u32 * window_ms) / node_count as u32
}
