//! The ring that decides whether an observation is worth transmitting.
//!
//! The ring is what keeps a node's airtime down, and airtime is the scarce thing on a
//! shared control channel: an access point beacons ten times a dwell and a node
//! returns to it every sweep. The store keeps every sighting it is given, so what the
//! ring suppresses is lost for good — which is why suppression is not permanent.
//!
//! An address held in the ring is reported again for two reasons, and no others:
//!
//! - **It has been [`DEDUP_REFRESH_MS`] since it was last reported.** Without this a
//!   node goes silent once it has reported its neighbourhood, and stays silent across
//!   host sessions because the ring outlives them (`docs/phase-4-findings.md`). A
//!   stationary node's quiet table then reads as a fault, and a second capture from
//!   the same spot is empty.
//! - **It is at least [`DEDUP_RSSI_GAIN_DB`] stronger than anything reported for it.**
//!   The export writes the strongest sighting's position, so a node driving towards an
//!   access point it first heard at the edge of range has a better fix to offer. The
//!   margin is wide enough that ordinary jitter between beacons does not cross it, and
//!   the baseline only ever rises, so an access point is re-reported for getting
//!   closer and never for wobbling.
//!
//! [`MacRing::clear`] empties the ring outright, for two further reasons:
//!
//! - **The node's job changed.** Entries built under the old share describe a
//!   neighbourhood the node no longer listens to and only take up slots.
//! - **The operator asked.** This gives a fresh capture from a stationary spot without
//!   waiting out the refresh or rebooting the node.
//!
//! Eviction is still oldest-*inserted* first, and a re-report updates its entry in
//! place rather than moving it to the front. A constantly-beaconing access point
//! therefore still ages out on schedule, and a busy neighbourhood refreshes itself
//! through the ring faster than the timer would.
//!
//! Time arrives as a `u32` of milliseconds rather than being read, for the same reason
//! the host engine takes `Now`: the rules are testable against a clock the test
//! invents. It wraps after 49 days and the comparisons wrap with it.
//!
//! # Why the ring is hashed
//!
//! The node checks the ring from its Wi-Fi receive callback, once for every beacon
//! and probe response it hears, inside a lock that holds interrupts off
//! (`esp-sync`'s `NonReentrantMutex`). Timed on the ESP32-C5 and C6, a linear scan of
//! a 256-entry ring there took 18-37 µs a frame. A hash lookup takes under 1 µs, and
//! measured the same at 512 entries as at 256, so the ring is sized for the
//! neighbourhood and the chip's RAM rather than for the lookup. The index is the shared
//! `mac_index` table, twice the ring's size in slots.
//!
//! That is why there are two sizes. [`C5DedupRing`] holds 512 addresses in 8 KB.
//! [`C6DedupRing`] holds 4,096 in 64 KB: the C6 has 128 KB more SRAM and no 5 GHz
//! radio, so in a mixed fleet it is the likely Bluetooth node, and the RAM comes out
//! of a main stack that has never used more than about 2 KB of what it is given.
//!
//! [`DEDUP_REFRESH_MS`]: crate::dedup::DEDUP_REFRESH_MS
//! [`DEDUP_RSSI_GAIN_DB`]: crate::dedup::DEDUP_RSSI_GAIN_DB

use crate::mac_index::MacIndex;

/// How many recently-reported addresses an ESP32-C5 node holds. See [`crate::dedup`].
pub const DEDUP_RING_C5: usize = 512;

/// Hash slots behind [`DEDUP_RING_C5`]. A power of two at least twice the ring, so the
/// index [`crate::dedup::MacRing`] builds over it is never more than half full.
pub const DEDUP_INDEX_C5: usize = 2 * DEDUP_RING_C5;

/// How many recently-reported addresses an ESP32-C6 node holds. See [`crate::dedup`].
///
/// Eight times the C5's. The C6 has 128 KB more SRAM and no 5 GHz radio, so in a
/// mixed fleet it is the node likely to be given the Bluetooth scan, and the room
/// costs it 64 KB of a main stack that has never used more than 2 KB.
pub const DEDUP_RING_C6: usize = 4096;

/// Hash slots behind [`DEDUP_RING_C6`], for the reason [`DEDUP_INDEX_C5`] gives.
pub const DEDUP_INDEX_C6: usize = 2 * DEDUP_RING_C6;

/// How long a node suppresses an address it has reported before reporting it again.
///
/// Five minutes: long enough that a stationary node's whole neighbourhood costs a few
/// sightings a minute, short enough that a fresh host session hears it without a reboot.
pub const DEDUP_REFRESH_MS: u32 = 5 * 60 * 1000;

/// How much stronger a sighting must be than any reported for its address to be
/// reported again before the refresh.
pub const DEDUP_RSSI_GAIN_DB: i8 = 10;

#[derive(Debug, Clone, Copy)]
struct Entry {
    mac: [u8; 6],
    /// When this address was last reported.
    at_ms: u32,
    /// The strongest signal reported for it since `at_ms` was last reset by a refresh.
    best_rssi: i8,
}

/// A fixed-capacity ring of recently reported addresses, oldest evicted first.
///
/// `N` is the number of addresses held; [`C5DedupRing`] and [`C6DedupRing`] are the
/// sizes the firmware uses. `S` is the number of hash slots behind it — a separate parameter
/// because stable Rust cannot write `[u16; 2 * N]` for a generic `N`.
#[derive(Debug, Clone)]
pub struct MacRing<const N: usize, const S: usize> {
    entries: [Entry; N],
    len: usize,
    cursor: usize,
    /// Where each held address sits in `entries`.
    index: MacIndex<S>,
}

/// The ring an ESP32-C5 node and its simulation use: [`DEDUP_RING_C5`] addresses
/// over [`DEDUP_INDEX_C5`] hash slots.
pub type C5DedupRing = MacRing<DEDUP_RING_C5, DEDUP_INDEX_C5>;

/// The ring an ESP32-C6 node and its simulation use: [`DEDUP_RING_C6`] addresses
/// over [`DEDUP_INDEX_C6`] hash slots.
pub type C6DedupRing = MacRing<DEDUP_RING_C6, DEDUP_INDEX_C6>;

impl<const N: usize, const S: usize> MacRing<N, S> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [Entry { mac: [0; 6], at_ms: 0, best_rssi: 0 }; N],
            len: 0,
            cursor: 0,
            index: MacIndex::new::<N>(),
        }
    }

    fn find(&self, mac: &[u8; 6]) -> Option<usize> {
        self.index.find(mac, |pos| self.entries[pos].mac)
    }

    /// Whether this address is held at all, due or not.
    #[must_use]
    pub fn contains(&self, mac: &[u8; 6]) -> bool {
        self.find(mac).is_some()
    }

    /// Whether a sighting of `mac` at `rssi` is worth transmitting at `now_ms`.
    ///
    /// Pass `None` for a reading with no signal strength (a BLE report's `127`), which
    /// can never count as stronger.
    #[must_use]
    pub fn is_due(&self, mac: &[u8; 6], rssi: Option<i8>, now_ms: u32) -> bool {
        let Some(entry) = self.find(mac).map(|i| &self.entries[i]) else { return true };
        now_ms.wrapping_sub(entry.at_ms) >= DEDUP_REFRESH_MS
            || rssi.is_some_and(|r| {
                i16::from(r) >= i16::from(entry.best_rssi) + i16::from(DEDUP_RSSI_GAIN_DB)
            })
    }

    /// Record that `mac` was reported at `now_ms`.
    ///
    /// Call it once the sighting is on the air, not before: suppressing an address the
    /// host never received hides it until the refresh. A held address is updated in
    /// place and keeps its place in the eviction order — and in the index, which an
    /// in-place update never touches.
    pub fn record(&mut self, mac: [u8; 6], rssi: Option<i8>, now_ms: u32) {
        let rssi = rssi.unwrap_or(i8::MIN);
        if let Some(i) = self.find(&mac) {
            let entry = &mut self.entries[i];
            if now_ms.wrapping_sub(entry.at_ms) >= DEDUP_REFRESH_MS {
                // A refresh starts the baseline again: the node may have moved since.
                *entry = Entry { mac, at_ms: now_ms, best_rssi: rssi };
            } else {
                entry.best_rssi = entry.best_rssi.max(rssi);
            }
            return;
        }
        if self.len == N {
            // The ring is full, so this insert overwrites `cursor`'s entry: unindex
            // it first, or the index would go on pointing a stale slot at the MAC
            // that now lives there.
            let evicted = self.entries[self.cursor].mac;
            self.index.remove(&evicted, |pos| self.entries[pos].mac);
        }
        self.entries[self.cursor] = Entry { mac, at_ms: now_ms, best_rssi: rssi };
        self.index.insert(&mac, self.cursor);
        self.cursor = (self.cursor + 1) % N;
        self.len = self.len.saturating_add(1).min(N);
    }

    /// [`Self::is_due`] and [`Self::record`] together, for a caller whose transmission
    /// cannot fail.
    pub fn offer(&mut self, mac: [u8; 6], rssi: Option<i8>, now_ms: u32) -> bool {
        let due = self.is_due(&mac, rssi, now_ms);
        if due {
            self.record(mac, rssi, now_ms);
        }
        due
    }

    /// How many addresses are held, up to `N`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been recorded yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Empty the ring outright, so every address held is due again.
    ///
    /// The entries themselves are left as they are — a lookup never reaches one
    /// through a cleared `index` — but `index` itself is reset to empty, since
    /// a stale slot would otherwise go on pointing at an entry this call means to
    /// forget. That is `S` halfwords, 2 KB at the firmware's size, and this only
    /// runs on a share change or an operator clear.
    pub fn clear(&mut self) {
        self.len = 0;
        self.cursor = 0;
        self.index.clear();
    }
}

impl<const N: usize, const S: usize> Default for MacRing<N, S> {
    fn default() -> Self {
        Self::new()
    }
}
