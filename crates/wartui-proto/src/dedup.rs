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
//! (`esp-sync`'s `NonReentrantMutex`). A linear scan of [`crate::plan::DEDUP_RING`]
//! entries there was estimated at 10-30 µs a frame; a hash lookup is one or two
//! probes. `index` is a linear-probing table over positions in `entries`, never more
//! than half full ([`crate::plan::DEDUP_INDEX`]) so probe runs stay short. MACs that
//! collide degrade to a probe run no longer than `N`, which is a linear scan and no
//! worse.
//!
//! [`DEDUP_REFRESH_MS`]: crate::plan::DEDUP_REFRESH_MS
//! [`DEDUP_RSSI_GAIN_DB`]: crate::plan::DEDUP_RSSI_GAIN_DB

use crate::plan::{DEDUP_INDEX, DEDUP_REFRESH_MS, DEDUP_RING, DEDUP_RSSI_GAIN_DB};

#[derive(Debug, Clone, Copy)]
struct Entry {
    mac: [u8; 6],
    /// When this address was last reported.
    at_ms: u32,
    /// The strongest signal reported for it since `at_ms` was last reset by a refresh.
    best_rssi: i8,
}

/// Marks an `index` slot as holding nothing. `N < u16::MAX` (checked in [`MacRing::new`])
/// is what keeps this from colliding with a real position in `entries`.
const EMPTY: u16 = u16::MAX;

/// A fixed-capacity ring of recently reported addresses, oldest evicted first.
///
/// `N` is the number of addresses held; [`DEDUP_RING`] is the size the firmware uses.
/// `S` is the number of hash slots behind it ([`DEDUP_INDEX`]) — a separate parameter
/// because stable Rust cannot write `[u16; 2 * N]` for a generic `N`.
#[derive(Debug, Clone)]
pub struct MacRing<const N: usize, const S: usize> {
    entries: [Entry; N],
    len: usize,
    cursor: usize,
    /// `index[hash(mac)..]`, probed linearly, holds the position of `mac` in
    /// `entries`, or [`EMPTY`].
    index: [u16; S],
}

/// The ring size the firmware and simulator use: [`DEDUP_RING`] addresses over
/// [`DEDUP_INDEX`] hash slots.
pub type DedupRing = MacRing<DEDUP_RING, DEDUP_INDEX>;

impl<const N: usize, const S: usize> MacRing<N, S> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        const {
            assert!(
                S.is_power_of_two() && S >= 2 * N && N < u16::MAX as usize,
                "the index must be a power of two, at least twice the ring, with room for EMPTY"
            );
        }
        Self {
            entries: [Entry { mac: [0; 6], at_ms: 0, best_rssi: 0 }; N],
            len: 0,
            cursor: 0,
            index: [EMPTY; S],
        }
    }

    /// Fold `mac` into a hash slot: the top bits of a multiplicative hash, all in
    /// 32-bit arithmetic, which suits RV32.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "S is a power of two, so trailing_zeros() is at most 31 and the shift below \
        always leaves a value under 32 bits"
    )]
    const fn hash(mac: &[u8; 6]) -> usize {
        let hi = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]);
        let lo = u32::from_be_bytes([0, 0, mac[4], mac[5]]);
        let h = (hi ^ lo).wrapping_mul(0x9E37_79B1);
        (h >> (32 - S.trailing_zeros())) as usize
    }

    /// The `index` slot holding `mac`'s position in `entries`, if any.
    fn find_slot(&self, mac: &[u8; 6]) -> Option<usize> {
        let mut slot = Self::hash(mac);
        loop {
            let pos = self.index[slot];
            if pos == EMPTY {
                return None;
            }
            if self.entries[usize::from(pos)].mac == *mac {
                return Some(slot);
            }
            slot = (slot + 1) % S;
        }
    }

    fn find(&self, mac: &[u8; 6]) -> Option<usize> {
        self.find_slot(mac).map(|slot| usize::from(self.index[slot]))
    }

    /// Point some `index` slot at `entries[pos]`, starting the probe at its home slot.
    fn insert_index(&mut self, mac: [u8; 6], pos: usize) {
        let mut slot = Self::hash(&mac);
        while self.index[slot] != EMPTY {
            slot = (slot + 1) % S;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "pos < N < u16::MAX, asserted in MacRing::new"
        )]
        {
            self.index[slot] = pos as u16;
        }
    }

    /// Remove `mac` from the index, backward-shifting later entries of its probe run
    /// forward so a later lookup does not stop early at the hole this leaves. No
    /// tombstones: the standard deletion for linear probing.
    fn unindex(&mut self, mac: [u8; 6]) {
        let Some(start) = self.find_slot(&mac) else { return };
        let mut hole = start;
        loop {
            self.index[hole] = EMPTY;
            let mut j = hole;
            loop {
                j = (j + 1) % S;
                let pos = self.index[j];
                if pos == EMPTY {
                    return;
                }
                let home = Self::hash(&self.entries[usize::from(pos)].mac);
                // Whether `home` lies cyclically in `(hole, j]`: if so, `j` is still
                // reachable from its own home slot without the hole and must stay.
                let pinned =
                    if hole <= j { home > hole && home <= j } else { home <= j || home > hole };
                if pinned {
                    continue;
                }
                self.index[hole] = pos;
                hole = j;
                break;
            }
        }
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
            self.unindex(self.entries[self.cursor].mac);
        }
        self.entries[self.cursor] = Entry { mac, at_ms: now_ms, best_rssi: rssi };
        self.insert_index(mac, self.cursor);
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
    /// forget. That's `S` halfwords, about 1 KB at the firmware's size, and this only
    /// runs on a share change or an operator clear.
    pub fn clear(&mut self) {
        self.len = 0;
        self.cursor = 0;
        self.index = [EMPTY; S];
    }
}

impl<const N: usize, const S: usize> Default for MacRing<N, S> {
    fn default() -> Self {
        Self::new()
    }
}
