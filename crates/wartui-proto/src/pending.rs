//! The buffers one dwell's sightings and one scan's reports wait in, and the count of what a
//! full one turns away.

use crate::beacon::Sighting;
use crate::hci::AdvReport;
use crate::mac_index::MacIndex;

/// The sightings of one dwell, one per BSSID, waiting for the main loop to drain them.
///
/// It never wraps: a full buffer turns the newest access point away and counts it in
/// [`Self::dropped`], once per dwell. Everything already held is a distinct access point
/// not yet reported, so evicting one would trade a certain observation for a possible one.
///
/// An access point beacons about ten times a dwell, and only its first sighting is kept.
/// Nothing is merged from the repeats, unlike [`crate::pending::BlePending`], whose
/// identifier can arrive after the first hearing: a beacon carries the whole record.
///
/// Slots go only to access points the caller says are due to be reported, for the
/// reason [`crate::pending::BlePending`] gives.
///
/// The BSSID lookup is hashed for the reason in [`crate::pending::BlePending`]'s "Why it is
/// hashed": [`Self::record`] runs once per beacon inside a lock that holds interrupts
/// off, and a [`Sighting`] is larger than the report a linear search there was timed on.
///
/// `N` is the number of sightings held and `S` the hash slots behind them: a power of
/// two at least `2 * N`, a separate parameter for the reason [`crate::dedup::MacRing`]
/// gives.
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
    /// A held BSSID is ignored without asking `due`, which saves the caller's lookup
    /// on every repeat beacon. `due` is asked before the buffer is checked for room,
    /// so a sighting that is not due neither takes a slot nor counts as dropped.
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
    /// The bound is `len` and not the array: the slots past `len` are an earlier
    /// dwell's leavings or the blank fill, and would go on the air as if heard.
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

    /// Access points turned away by a full buffer since construction, each counted
    /// once per dwell however often it beacons. Wraps. See [`Refused`] for how it
    /// slightly undercounts.
    #[must_use]
    pub const fn dropped(&self) -> u16 {
        self.dropped.total()
    }
}

/// The reports of one scan, one per address, waiting for the main loop to drain them.
///
/// It never wraps: a full buffer turns the newest address away and counts it in
/// [`Self::dropped`], once per scan, the same policy the Wi-Fi sightings follow.
///
/// Slots go only to addresses the caller says are due to be reported. An
/// advertiser the host already has is heard again on every scan; given a slot,
/// it is thrown away by the drain, and a crowded room fills the buffer with
/// such repeats and turns new advertisers away.
///
/// # Why it is hashed
///
/// [`Self::record`] runs once per advertising report, not once per advertiser, and the
/// node calls it inside a lock that holds interrupts off (`esp-sync`'s
/// `NonReentrantMutex`). Timed on the ESP32-C5 and C6, a linear search of the held
/// reports costs about 135 ns an entry: 11-12 µs worst case at 80 reports and 17-18 µs
/// at 128, the cost [`crate::dedup`] hashed its ring to be rid of. The same index finds
/// an address in one or two probes whatever `N` is.
///
/// `N` is the number of reports held and `S` the hash slots behind them: a power of two
/// at least `2 * N`, a separate parameter for the reason [`crate::dedup::MacRing`] gives.
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
    /// A held address merges unconditionally, without asking `due`: the
    /// identifier can arrive in a later packet than the first hearing, and an
    /// advertiser that led with its flags and followed with its manufacturer
    /// data is still the one advertiser. `due` is asked only when the report
    /// would take a new slot, and before the buffer is checked for room, so a
    /// report that is not due neither takes a slot nor counts as dropped.
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
    /// The bound is `len` and not the array: the slots past `len` are an
    /// earlier scan's leavings or the zero fill — a `00:00:00:00:00:00`
    /// advertiser at 0 dBm that would go on the air as if heard.
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

    /// Advertisers turned away by a full buffer since construction, each counted
    /// once per scan however often it repeats. Wraps. See [`Refused`] for how it
    /// slightly undercounts.
    #[must_use]
    pub const fn dropped(&self) -> u16 {
        self.dropped.total()
    }
}

/// Addresses a full buffer turned away, counted once per dwell or scan.
///
/// A pending buffer that is full turns an address away on every frame it sends, and an
/// access point beacons about ten times a dwell, so a count of refused packets measures
/// how loud the neighbourhood is rather than how many networks went unreported. This
/// counts each address once until [`Self::reset`], which the caller runs at the start of
/// each dwell or scan.
///
/// [`Self::note`] runs inside the Wi-Fi receive callback's lock, so its cost is fixed:
/// one hash and one bit, however many addresses are refused. The price is that two
/// addresses sharing one of the 1024 bits in a dwell count once — about 1 in 50 at 50
/// refusals — so the total slightly undercounts. The hash is fixed; radio addresses are
/// not an adversary worth defending a diagnostic count against.
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
