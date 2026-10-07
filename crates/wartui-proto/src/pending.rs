//! The buffers a node's sightings wait in until the main loop sends them, and the count of
//! what a full one turns away. Only the node firmware uses this; the crate docs say why it
//! lives here.
//!
//! [`WifiPending`] holds one dwell's access points and [`BlePending`] one scan's
//! advertisers. Both follow the same rules:
//!
//! - **One entry per address.** A repeat hearing never takes a second slot.
//! - **Slots go only to addresses the caller says are due.** An address the host already
//!   has is heard again every dwell or scan. Given a slot, it is thrown away by the drain,
//!   and a crowded neighbourhood fills the buffer with such repeats and turns new
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

/// The sightings of one dwell, one per BSSID. The module docs give the rules it follows.
///
/// An access point beacons about ten times a dwell, and only its first sighting is kept.
/// A beacon carries the whole record, so nothing is merged from the repeats.
#[derive(Debug, Clone)]
pub struct WifiPending<const N: usize, const S: usize> {
    items: [Sighting; N],
    len: usize,
    taken: usize,
    dropped: Refused,
    /// Where each held BSSID sits in `items`. Nothing is ever evicted, so entries
    /// leave it only all at once, in [`Self::clear`].
    index: MacIndex<S>,
}

impl<const N: usize, const S: usize> Default for WifiPending<N, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const S: usize> WifiPending<N, S> {
    /// An empty buffer, `const` so it can sit in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: [Sighting::BLANK; N],
            len: 0,
            taken: 0,
            dropped: Refused::new(),
            index: MacIndex::new::<N>(),
        }
    }

    /// Keep `sighting` if its BSSID is new this dwell and `due`.
    ///
    /// A held BSSID is ignored without asking `due`, which saves the caller's lookup on
    /// every repeat beacon. `due` is asked before the room check.
    pub fn record(&mut self, sighting: Sighting, due: impl FnOnce(&Sighting) -> bool) {
        if self.index.find(&sighting.bssid, |pos| self.items[pos].bssid).is_some() {
            return;
        }
        if !due(&sighting) {
            return;
        }
        if self.len == N {
            self.dropped.note(&sighting.bssid);
            return;
        }
        self.items[self.len] = sighting;
        self.index.insert(&sighting.bssid, self.len);
        self.len += 1;
    }

    /// Take the oldest sighting not yet taken, if there is one.
    ///
    /// The bound is `len`, not the array. The slots past `len` are an earlier dwell's
    /// leavings or the blank fill, and would go on the air as if heard.
    pub fn take(&mut self) -> Option<Sighting> {
        if self.taken >= self.len {
            return None;
        }
        let sighting = self.items[self.taken];
        self.taken += 1;
        Some(sighting)
    }

    /// Empty the buffer for a new dwell. [`Self::dropped`] carries on, and an access
    /// point turned away last dwell counts again if it is turned away in this one.
    pub fn clear(&mut self) {
        self.len = 0;
        self.taken = 0;
        self.dropped.reset();
        self.index.clear();
    }

    /// Distinct BSSIDs held this dwell, at most `N`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this dwell holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Access points a full buffer turned away since construction, each once per dwell.
    /// Wraps. [`Refused`] says how it slightly undercounts.
    #[must_use]
    pub const fn dropped(&self) -> u16 {
        self.dropped.total()
    }
}

/// The reports of one scan, one per address. The module docs give the rules it follows.
///
/// Unlike [`WifiPending`], a repeat merges into the held report, because an advertiser's
/// identifier can arrive after its first hearing.
#[derive(Debug, Clone)]
pub struct BlePending<const N: usize, const S: usize> {
    items: [AdvReport; N],
    len: usize,
    taken: usize,
    dropped: Refused,
    /// Where each held address sits in `items`. Nothing is ever evicted, so entries
    /// leave it only all at once, in [`Self::clear`].
    index: MacIndex<S>,
}

impl<const N: usize, const S: usize> Default for BlePending<N, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const S: usize> BlePending<N, S> {
    /// An empty buffer, `const` so it can sit in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: [AdvReport { address: [0; 6], rssi: 0, mfgr: None }; N],
            len: 0,
            taken: 0,
            dropped: Refused::new(),
            index: MacIndex::new::<N>(),
        }
    }

    /// Keep `report` if its address is new and `due`, or merge it into the
    /// reading already held for that address.
    ///
    /// A held address merges without asking `due`. An advertiser that led with its
    /// flags and followed with its manufacturer data is still one advertiser. `due` is
    /// asked only when the report would take a new slot, and before the room check.
    pub fn record(&mut self, report: AdvReport, due: impl FnOnce(&AdvReport) -> bool) {
        if !report.has_rssi() {
            return;
        }
        if let Some(i) = self.index.find(&report.address, |pos| self.items[pos].address) {
            let held = &mut self.items[i];
            held.rssi = held.rssi.max(report.rssi);
            if held.mfgr.is_none() {
                held.mfgr = report.mfgr;
            }
            return;
        }
        if !due(&report) {
            return;
        }
        if self.len == N {
            self.dropped.note(&report.address);
            return;
        }
        self.items[self.len] = report;
        self.index.insert(&report.address, self.len);
        self.len += 1;
    }

    /// Take the oldest report not yet taken, if there is one.
    ///
    /// The bound is `len`, not the array. The slots past `len` are an earlier scan's
    /// leavings or the zero fill, a `00:00:00:00:00:00` advertiser at 0 dBm that would
    /// go on the air as if heard.
    pub fn take(&mut self) -> Option<AdvReport> {
        if self.taken >= self.len {
            return None;
        }
        let report = self.items[self.taken];
        self.taken += 1;
        Some(report)
    }

    /// Empty the buffer for a new scan. [`Self::dropped`] carries on, and an
    /// advertiser turned away last scan counts again if it is turned away in this one.
    ///
    /// Resetting the index writes all `S` slots, which is cheap at once per scan.
    pub fn clear(&mut self) {
        self.len = 0;
        self.taken = 0;
        self.dropped.reset();
        self.index.clear();
    }

    /// Distinct addresses held this scan, at most `N`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this scan holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Advertisers a full buffer turned away since construction, each once per scan.
    /// Wraps. [`Refused`] says how it slightly undercounts.
    #[must_use]
    pub const fn dropped(&self) -> u16 {
        self.dropped.total()
    }
}

/// Addresses a full buffer turned away, counted once per dwell or scan.
///
/// A full buffer turns an address away on every frame it sends, and an access point
/// beacons about ten times a dwell. A count of refused frames would measure how loud the
/// neighbourhood is, not how many addresses went unreported. This counts each address
/// once until [`Self::reset`], which the caller runs at the start of each dwell or scan.
///
/// [`Self::note`] runs inside the Wi-Fi receive callback's lock, so its cost is fixed:
/// one hash and one bit. Two addresses that share one of the 1024 bits in a dwell count
/// once, about 1 in 50 at 50 refusals, so the total slightly undercounts. The hash is
/// fixed; radio addresses are not an adversary worth defending a diagnostic count
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
