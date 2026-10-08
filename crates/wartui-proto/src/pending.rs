//! The buffers a node's sightings wait in until the main loop sends them, and the count of
//! what a full one turns away. Only the node firmware uses this; the crate docs say why it
//! lives here.
//!
//! [`Pending`] is the buffer. [`WifiPending`] holds one dwell's access points in it and
//! [`BlePending`] one scan's advertisers. They differ only in what the crate-private
//! `Entry` trait supplies for each, and both follow the same rules:
//!
//! - **One entry per address.** A repeat hearing never takes a second slot.
//! - **Slots go only to addresses the caller says are due.** An address the host already
//!   has is heard again every dwell or scan. Given a slot, it is thrown away by the drain,
//!   and a crowded neighborhood fills the buffer with such repeats and turns new
//!   addresses away. The `due` check runs before the room check, so an address that is
//!   not due neither takes a slot nor counts as dropped.
//! - **A full buffer never wraps.** It turns the newest address away and counts it in
//!   [`Refused`], once per dwell or scan. Everything held is a distinct address not yet
//!   reported, so evicting one would trade a certain sighting for a possible one.
//!
//! `N` is the number of entries held. `S` is the hash slots behind them, a power of two at
//! least `2 * N`, separate for the reason [`crate::dedup::MacRing`] gives.
//!
//! # Why the lookup is hashed
//!
//! `record` runs once per beacon or advertising report, not once per address, inside a
//! lock that holds interrupts off (`esp-sync`'s `NonReentrantMutex`). Timed on the
//! ESP32-C5 and C6, a linear search of held BLE reports costs about 135 ns an entry:
//! 11-12 µs worst case at 80 reports and 17-18 µs at 128. A [`Sighting`] is larger than
//! the report that was timed, so a linear Wi-Fi search costs more. The hash index finds an
//! address in one or two probes whatever `N` is.

use crate::beacon::Sighting;
use crate::hci::AdvReport;
use crate::mac_index::MacIndex;

use entry::Entry;

/// `pub` in a private module, so no other crate can name, call or implement it. The blank
/// fill and the merge rules are the buffer's business, not API, and no outside type has a
/// reason to be buffered.
mod entry {
    /// What a [`Pending`](super::Pending) holds: the part of a buffer's rules that depends
    /// on the kind of hearing.
    pub trait Entry: Copy {
        /// The fill for slots nothing has taken. A constant rather than `Default`, so
        /// [`Pending::new`](super::Pending::new) stays a `const fn` and the buffer can sit
        /// in a `static`.
        const BLANK: Self;

        /// The address the buffer keeps one entry for.
        fn address(&self) -> [u8; 6];

        /// Whether the entry is worth holding at all. An entry ruled out here is never
        /// looked up, merged, asked `due` or counted as dropped, since it is no reading of
        /// anything.
        fn is_usable(&self) -> bool {
            true
        }

        /// Fold a repeat hearing of the same address into the held entry. Nothing by
        /// default: the first hearing is kept as it is.
        fn merge(&mut self, _repeat: &Self) {}
    }
}

/// An access point beacons about ten times a dwell, and only its first sighting is kept.
/// A beacon carries the whole record, so nothing is merged from the repeats.
impl Entry for Sighting {
    const BLANK: Self = Sighting::BLANK;

    fn address(&self) -> [u8; 6] {
        self.bssid
    }
}

/// An advertiser's identifier can arrive after its first hearing, so a repeat merges into
/// the held report. A report without a reading is not held.
impl Entry for AdvReport {
    const BLANK: Self = AdvReport::BLANK;

    fn address(&self) -> [u8; 6] {
        self.address
    }

    fn is_usable(&self) -> bool {
        self.has_rssi()
    }

    /// The strongest reading wins. The first identifier stays, because a later one is
    /// not a better reading of it.
    fn merge(&mut self, repeat: &Self) {
        self.rssi = self.rssi.max(repeat.rssi);
        if self.mfgr.is_none() {
            self.mfgr = repeat.mfgr;
        }
    }
}

/// One dwell's access points, one per BSSID. The first sighting of a BSSID is kept whole,
/// and repeats are ignored.
pub type WifiPending<const N: usize, const S: usize> = Pending<Sighting, N, S>;

/// One scan's advertisers, one per address. A repeat merges into the held report, keeping
/// the strongest RSSI and the first manufacturer ID. A report without an RSSI reading is not
/// held.
pub type BlePending<const N: usize, const S: usize> = Pending<AdvReport, N, S>;

/// The entries of one dwell or scan, one per address. The module docs give the rules it
/// follows.
///
/// `T` is only ever `Sighting` or `AdvReport`, used through [`WifiPending`] and
/// [`BlePending`]. The per-entry rules are crate-private.
#[derive(Debug, Clone)]
pub struct Pending<T: Entry, const N: usize, const S: usize> {
    items: [T; N],
    len: usize,
    taken: usize,
    dropped: Refused,
    /// Where each held address sits in `items`. Nothing is ever evicted, so entries
    /// leave it only all at once, in [`Self::clear`].
    index: MacIndex<S>,
}

impl<T: Entry, const N: usize, const S: usize> Default for Pending<T, N, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Entry, const N: usize, const S: usize> Pending<T, N, S> {
    /// An empty buffer, `const` so it can sit in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: [T::BLANK; N],
            len: 0,
            taken: 0,
            dropped: Refused::new(),
            index: MacIndex::new::<N>(),
        }
    }

    /// Keep `entry` if its address is new this dwell or scan and `due`, or merge it into
    /// the entry already held for that address.
    ///
    /// A held address merges without asking `due`, which saves the caller's lookup on
    /// every repeat hearing. `due` is asked only when the entry would take a new slot,
    /// and before the room check.
    pub fn record(&mut self, entry: T, due: impl FnOnce(&T) -> bool) {
        if !entry.is_usable() {
            return;
        }
        let address = entry.address();
        if let Some(i) = self.index.find(&address, |pos| self.items[pos].address()) {
            self.items[i].merge(&entry);
            return;
        }
        if !due(&entry) {
            return;
        }
        if self.len == N {
            self.dropped.note(&address);
            return;
        }
        self.items[self.len] = entry;
        self.index.insert(&address, self.len);
        self.len += 1;
    }

    /// Take the oldest entry not yet taken, if there is one.
    ///
    /// The bound is `len`, not the array. The slots past `len` are an earlier dwell's or
    /// scan's leavings or the blank fill, and would go on the air as if heard.
    pub fn take(&mut self) -> Option<T> {
        if self.taken >= self.len {
            return None;
        }
        let entry = self.items[self.taken];
        self.taken += 1;
        Some(entry)
    }

    /// Empty the buffer for a new dwell or scan. [`Self::dropped`] carries on, and an
    /// address turned away last time counts again if it is turned away in this one.
    ///
    /// Resetting the index writes all `S` slots, which is cheap at once per dwell or scan.
    pub fn clear(&mut self) {
        self.len = 0;
        self.taken = 0;
        self.dropped.reset();
        self.index.clear();
    }

    /// Distinct addresses held this dwell or scan, at most `N`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this dwell or scan holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Addresses a full buffer turned away since construction, each once per dwell or
    /// scan. Wraps. [`Refused`] says how it slightly undercounts.
    #[must_use]
    pub const fn dropped(&self) -> u16 {
        self.dropped.total()
    }
}

/// Addresses a full buffer turned away, counted once per dwell or scan.
///
/// A full buffer turns an address away on every frame it sends, and an access point
/// beacons about ten times a dwell. A count of refused frames would measure how loud the
/// neighborhood is, not how many addresses went unreported. This counts each address
/// once until [`Self::reset`], which the caller runs at the start of each dwell or scan.
///
/// [`Self::note`] runs inside the lock both pending buffers record under, so its cost is
/// fixed: one hash and one bit. Two addresses that share one of the 1024 bits in a dwell
/// count once, about 1 in 50 at 50 refusals, so the total slightly undercounts. The hash
/// is fixed; radio addresses are not an adversary worth defending a diagnostic count
/// against.
#[derive(Debug, Clone)]
pub struct Refused {
    /// One bit per hash of an address refused since the last [`Self::reset`].
    seen: [u32; 32],
    /// Addresses counted since construction. Wraps.
    total: u16,
}

impl Refused {
    /// Nothing refused, `const` so it can sit in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self { seen: [0; 32], total: 0 }
    }

    /// Count `addr` as refused, unless it already was since the last [`Self::reset`].
    pub fn note(&mut self, addr: &[u8; 6]) {
        // FNV-1a over the address, folded to the bitset's 10 bits.
        let mut h: u32 = 0x811C_9DC5;
        for &b in addr {
            h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
        }
        let bit = (h ^ (h >> 10) ^ (h >> 20)) & 0x3FF;
        let word = &mut self.seen[(bit >> 5) as usize];
        let mask = 1 << (bit & 31);
        if *word & mask == 0 {
            *word |= mask;
            self.total = self.total.wrapping_add(1);
        }
    }

    /// Start a new dwell or scan: every address counts again. The total carries on.
    pub fn reset(&mut self) {
        self.seen = [0; 32];
    }

    /// Addresses refused since construction, once per dwell or scan each. Wraps.
    #[must_use]
    pub const fn total(&self) -> u16 {
        self.total
    }
}

impl Default for Refused {
    fn default() -> Self {
        Self::new()
    }
}
