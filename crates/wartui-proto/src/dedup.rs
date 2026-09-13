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
//! Eviction is still oldest-*inserted* first, and a re-report updates its entry in
//! place rather than moving it to the front. A constantly-beaconing access point
//! therefore still ages out on schedule, and a busy neighbourhood refreshes itself
//! through the ring faster than the timer would.
//!
//! Time arrives as a `u32` of milliseconds rather than being read, for the same reason
//! the host engine takes `Now`: the rules are testable against a clock the test
//! invents. It wraps after 49 days and the comparisons wrap with it.
//!
//! [`DEDUP_REFRESH_MS`]: crate::plan::DEDUP_REFRESH_MS
//! [`DEDUP_RSSI_GAIN_DB`]: crate::plan::DEDUP_RSSI_GAIN_DB

use crate::plan::{DEDUP_REFRESH_MS, DEDUP_RSSI_GAIN_DB};

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
/// `N` is the number of addresses held; [`crate::plan::DEDUP_RING`] is the size
/// the firmware uses.
#[derive(Debug, Clone)]
pub struct MacRing<const N: usize> {
    entries: [Entry; N],
    len: usize,
    cursor: usize,
}

impl<const N: usize> MacRing<N> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self { entries: [Entry { mac: [0; 6], at_ms: 0, best_rssi: 0 }; N], len: 0, cursor: 0 }
    }

    fn find(&self, mac: &[u8; 6]) -> Option<usize> {
        self.entries[..self.len].iter().position(|e| e.mac == *mac)
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
    /// place and keeps its place in the eviction order.
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
        self.entries[self.cursor] = Entry { mac, at_ms: now_ms, best_rssi: rssi };
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
}

impl<const N: usize> Default for MacRing<N> {
    fn default() -> Self {
        Self::new()
    }
}
