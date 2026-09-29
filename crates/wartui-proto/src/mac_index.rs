//! A linear-probing hash index from MAC address to a position in somebody else's array.
//!
//! [`crate::dedup::MacRing`], [`crate::hci::BlePending`] and [`crate::beacon::WifiPending`]
//! each look an address up from inside a lock that holds interrupts off, once per frame
//! or report heard, and a linear scan there costs microseconds per lookup that the
//! radio's callback does not have. Each keeps its entries where they are and builds this
//! index over them, so the index stores only positions and asks its owner, through a
//! `mac_at` closure, which MAC a position holds.
//!
//! Positions are `u16` and [`EMPTY`] is `u16::MAX`, which halves the index against
//! `usize` and is why [`MacIndex::new`] demands fewer than `u16::MAX` entries. The table
//! is never more than half full, so probe runs stay short; MACs that collide degrade to a
//! probe run no longer than the entries held, which is a linear scan and no worse.
//! Deletion is backward-shift rather than tombstones, so a table that churns forever
//! never fills with dead slots.

/// Marks a slot as holding nothing.
const EMPTY: u16 = u16::MAX;

/// Hash slots over the positions of an owner's entries. `S` is the slot count, a
/// separate parameter because stable Rust cannot write `[u16; 2 * N]` for a generic `N`.
#[derive(Debug, Clone)]
pub(crate) struct MacIndex<const S: usize> {
    /// `slots[hash(mac)..]`, probed linearly, holds the position of `mac` in the
    /// owner's entries, or [`EMPTY`].
    slots: [u16; S],
}

impl<const S: usize> MacIndex<S> {
    /// An empty index for an owner holding at most `N` entries.
    pub(crate) const fn new<const N: usize>() -> Self {
        const {
            assert!(
                S.is_power_of_two() && S >= 2 * N && N < u16::MAX as usize,
                "the index must be a power of two, at least twice the entries, with room for EMPTY"
            );
        }
        Self { slots: [EMPTY; S] }
    }

    /// Fold `mac` into a slot: the top bits of a multiplicative hash, all in 32-bit
    /// arithmetic, which suits RV32.
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

    /// The slot holding `mac`'s position, if any.
    fn find_slot(&self, mac: &[u8; 6], mac_at: impl Fn(usize) -> [u8; 6]) -> Option<usize> {
        let mut slot = Self::hash(mac);
        loop {
            let pos = self.slots[slot];
            if pos == EMPTY {
                return None;
            }
            if mac_at(usize::from(pos)) == *mac {
                return Some(slot);
            }
            slot = (slot + 1) % S;
        }
    }

    /// The position of `mac` in the owner's entries, if it is indexed.
    pub(crate) fn find(&self, mac: &[u8; 6], mac_at: impl Fn(usize) -> [u8; 6]) -> Option<usize> {
        self.find_slot(mac, mac_at).map(|slot| usize::from(self.slots[slot]))
    }

    /// Index `mac` at `pos`, starting the probe at its home slot. The caller has
    /// checked that `mac` is not indexed already.
    pub(crate) fn insert(&mut self, mac: &[u8; 6], pos: usize) {
        let mut slot = Self::hash(mac);
        while self.slots[slot] != EMPTY {
            slot = (slot + 1) % S;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "pos < N < u16::MAX, asserted in MacIndex::new"
        )]
        {
            self.slots[slot] = pos as u16;
        }
    }

    /// Remove `mac`, backward-shifting later entries of its probe run forward so a
    /// later lookup does not stop early at the hole this leaves.
    pub(crate) fn remove(&mut self, mac: &[u8; 6], mac_at: impl Fn(usize) -> [u8; 6]) {
        let Some(start) = self.find_slot(mac, &mac_at) else { return };
        let mut hole = start;
        loop {
            self.slots[hole] = EMPTY;
            let mut j = hole;
            loop {
                j = (j + 1) % S;
                let pos = self.slots[j];
                if pos == EMPTY {
                    return;
                }
                let home = Self::hash(&mac_at(usize::from(pos)));
                // Whether `home` lies cyclically in `(hole, j]`: if so, `j` is still
                // reachable from its own home slot without the hole and must stay.
                let pinned =
                    if hole <= j { home > hole && home <= j } else { home <= j || home > hole };
                if pinned {
                    continue;
                }
                self.slots[hole] = pos;
                hole = j;
                break;
            }
        }
    }

    /// Forget every position.
    pub(crate) fn clear(&mut self) {
        self.slots = [EMPTY; S];
    }
}

#[cfg(test)]
mod tests {
    use super::MacIndex;

    /// The owner's entries: position `p` holds `macs[p]`.
    fn at(macs: &[[u8; 6]]) -> impl Fn(usize) -> [u8; 6] + '_ {
        |p| macs[p]
    }

    fn mac(n: u16) -> [u8; 6] {
        let [hi, lo] = n.to_be_bytes();
        [0x02, 0, 0, 0, hi, lo]
    }

    /// Four MACs sharing one home slot of a `MacIndex<S>`.
    fn colliding<const S: usize>() -> [[u8; 6]; 4] {
        let mut out = [[0; 6]; 4];
        let mut found = 0;
        let home = MacIndex::<S>::hash(&mac(0));
        for n in 0..u16::MAX {
            if MacIndex::<S>::hash(&mac(n)) == home {
                out[found] = mac(n);
                found += 1;
                if found == out.len() {
                    return out;
                }
            }
        }
        panic!("too few collisions");
    }

    #[test]
    fn mac_index_finds_every_position_when_macs_share_a_home_slot() {
        let macs = colliding::<16>();
        let mut index = MacIndex::<16>::new::<4>();
        for (p, m) in macs.iter().enumerate() {
            assert_eq!(index.find(m, at(&macs)), None);
            index.insert(m, p);
        }
        for (p, m) in macs.iter().enumerate() {
            assert_eq!(index.find(m, at(&macs)), Some(p));
        }
    }

    #[test]
    fn mac_index_keeps_rest_of_probe_run_when_middle_is_removed() {
        let macs = colliding::<16>();
        let mut index = MacIndex::<16>::new::<4>();
        for (p, m) in macs.iter().enumerate() {
            index.insert(m, p);
        }
        index.remove(&macs[1], at(&macs));
        assert_eq!(index.find(&macs[1], at(&macs)), None);
        for p in [0, 2, 3] {
            assert_eq!(index.find(&macs[p], at(&macs)), Some(p), "position {p} is still reachable");
        }
    }

    #[test]
    fn mac_index_ignores_removal_when_mac_is_not_indexed() {
        let macs = [mac(1), mac(2)];
        let mut index = MacIndex::<8>::new::<2>();
        index.insert(&macs[0], 0);
        index.remove(&macs[1], at(&macs));
        assert_eq!(index.find(&macs[0], at(&macs)), Some(0));
    }

    #[test]
    fn mac_index_finds_nothing_when_cleared() {
        let macs = [mac(1), mac(2)];
        let mut index = MacIndex::<8>::new::<2>();
        index.insert(&macs[0], 0);
        index.insert(&macs[1], 1);
        index.clear();
        assert_eq!(index.find(&macs[0], at(&macs)), None);
        assert_eq!(index.find(&macs[1], at(&macs)), None);
    }
}
