//! How many distinct addresses a session has heard, in memory that does not grow with it.
//!
//! Answering exactly means holding every address heard, and a set of them is the one
//! thing in the process that would grow with the drive: about 18 bytes an address, 9 MiB
//! for a day's 500 k networks and 42 MiB for a twenty-node burst
//! (`docs/store-io-findings.md`). The store cannot answer instead without undoing what
//! makes capture cheap. A `COUNT(DISTINCT bssid)` scans and sorts a table that has no
//! index, on the card, and its read snapshot holds back the checkpoint that keeps the WAL
//! one commit long. A table or index keyed by address is a random-key B-tree, which the
//! schema has no room for at fourteen times the writes.
//!
//! So the figure is an estimate: a HyperLogLog of 2^14 one-byte registers, 16 KiB whatever
//! the session hears, with a standard error of about 0.8%. Below a few tens of thousands
//! of addresses linear counting takes over, and the figure is off by a handful. It is a
//! number for a person watching a capture; the store still holds every sighting, and
//! export counts networks exactly.

use std::hash::{DefaultHasher, Hash, Hasher};

use wartui_proto::link::Mac;

/// Bits of the hash that choose a register.
const PRECISION: u32 = 14;

/// Registers, and so bytes, held.
pub const REGISTERS: usize = 1 << PRECISION;

/// A cardinality estimate over addresses, in fixed memory.
#[derive(Debug, Clone)]
pub struct Distinct {
    registers: Box<[u8]>,
}

impl Default for Distinct {
    fn default() -> Self {
        Self::new()
    }
}

impl Distinct {
    /// An estimator that has heard nothing.
    #[must_use]
    pub fn new() -> Self {
        Self { registers: vec![0; REGISTERS].into_boxed_slice() }
    }

    /// Note one address. Hearing it again changes nothing.
    pub fn insert(&mut self, mac: &Mac) {
        // `DefaultHasher::new()` has fixed keys, so the same address always lands in the
        // same register. Nothing here is persisted, so a change of algorithm across Rust
        // releases would cost nothing.
        let mut hasher = DefaultHasher::new();
        mac.hash(&mut hasher);
        let hash = hasher.finish();
        let index = (hash >> (u64::BITS - PRECISION)) as usize;
        let rest = hash << PRECISION;
        // The run of zeros in what is left, plus one, capped at the bits there are.
        let rank = (rest.leading_zeros() + 1).min(u64::BITS - PRECISION + 1) as u8;
        let register = &mut self.registers[index];
        *register = (*register).max(rank);
    }

    /// Roughly how many distinct addresses have been inserted.
    #[must_use]
    pub fn estimate(&self) -> u64 {
        let m = REGISTERS as f64;
        let mut sum = 0.0;
        let mut zeros = 0usize;
        for &rank in &*self.registers {
            sum += 2f64.powi(-i32::from(rank));
            if rank == 0 {
                zeros += 1;
            }
        }
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let raw = alpha * m * m / sum;
        // Small counts leave registers empty, where the raw estimate is biased high and
        // counting the empties is far closer. A 64-bit hash needs no correction at the
        // top end.
        let estimate = if raw <= 2.5 * m && zeros > 0 { m * (m / zeros as f64).ln() } else { raw };
        estimate.round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic, locally-administered address for `n`.
    fn mac(n: u32) -> Mac {
        let [a, b, c, d] = n.to_be_bytes();
        [0x02, 0x00, a, b, c, d]
    }

    fn within(estimate: u64, truth: u64, fraction: f64) -> bool {
        (estimate as f64 - truth as f64).abs() <= truth as f64 * fraction
    }

    #[test]
    fn nothing_heard_reads_zero() {
        assert_eq!(Distinct::new().estimate(), 0);
    }

    #[test]
    fn one_address_heard_many_times_reads_one() {
        let mut distinct = Distinct::new();
        for _ in 0..10_000 {
            distinct.insert(&mac(7));
        }
        assert_eq!(distinct.estimate(), 1);
    }

    #[test]
    fn small_counts_read_within_a_handful() {
        // Linear counting's own noise is about five addresses at a thousand, so a
        // bound much tighter than this would be testing the hash, not the estimator.
        let mut distinct = Distinct::new();
        for n in 0..1_000 {
            distinct.insert(&mac(n));
            let truth = u64::from(n) + 1;
            let estimate = distinct.estimate();
            assert!(estimate.abs_diff(truth) <= truth / 50 + 2, "{estimate} for {truth}");
        }
    }

    #[test]
    fn a_drive_and_a_burst_read_within_two_percent_in_the_same_memory() {
        let mut distinct = Distinct::new();
        for n in 0..2_000_000 {
            distinct.insert(&mac(n));
            if n + 1 == 500_000 {
                let estimate = distinct.estimate();
                assert!(within(estimate, 500_000, 0.02), "{estimate} for a drive's 500 k");
            }
        }
        let estimate = distinct.estimate();
        assert!(within(estimate, 2_000_000, 0.02), "{estimate} for 2 M");
        assert_eq!(distinct.registers.len(), REGISTERS);
    }
}
