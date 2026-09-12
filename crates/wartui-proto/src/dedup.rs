//! The ring that decides whether an observation is worth transmitting.
//!
//! A node reports a BSSID once and suppresses it until enough other addresses have
//! pushed it out (`save_mac` / `seen_mac`, `src/WiFiOps.cpp:1803-1831`). Nothing
//! clears it at runtime, which is why a fleet's observation stream goes quiet a few
//! minutes into a run rather than repeating itself.
//!
//! Reproduced rather than improved on: the store dedups again on the host and could
//! absorb repeats, but the ring is also what keeps a node's airtime down, and airtime
//! is the scarce thing on a shared control channel.
//!
//! One correction — the firmware's array starts zeroed and tracks no length, so an
//! access point at `00:00:00:00:00:00` reads as already-seen from boot. Counting
//! entries costs a `usize` and removes the special case.

/// A fixed-capacity ring of recently reported addresses, oldest evicted first.
///
/// `N` is the number of addresses held; [`crate::plan::DEDUP_RING`] is the size
/// the firmware uses.
#[derive(Debug, Clone)]
pub struct MacRing<const N: usize> {
    macs: [[u8; 6]; N],
    len: usize,
    cursor: usize,
}

impl<const N: usize> MacRing<N> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self { macs: [[0; 6]; N], len: 0, cursor: 0 }
    }

    /// Whether this address is still being suppressed.
    #[must_use]
    pub fn contains(&self, mac: &[u8; 6]) -> bool {
        self.macs[..self.len].contains(mac)
    }

    /// Record `mac`, and say whether this is the first time it has been seen.
    ///
    /// `false` means the caller should drop the observation. A repeat does not move the
    /// address back to the front: the firmware's `seen_mac` returns before touching the
    /// cursor, so a constantly-beaconing access point still ages out on schedule — which
    /// is what keeps a stationary node's stream from dying completely.
    pub fn insert(&mut self, mac: [u8; 6]) -> bool {
        if self.contains(&mac) {
            return false;
        }
        self.macs[self.cursor] = mac;
        self.cursor = (self.cursor + 1) % N;
        self.len = self.len.saturating_add(1).min(N);
        true
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
