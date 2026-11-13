//! `run --expect-encrypted`: each attachment must see the bridge report its Bluetooth link
//! encrypted and bonded before the run sends load, and within [`SECURE_WITHIN`] of attaching.
//!
//! The P4 reports the link in `SpikeStats`, from the C6's latest `BleState`. The C6 forwards the
//! host stream only to a bonded, encrypted central, so an unbonded link sees no `SpikeStats` at
//! all, and times out rather than reading as refused.

use std::time::{Duration, Instant};

use wartui_proto::spike::SpikeStats;

/// How long an attachment waits for the bridge to report the link encrypted and bonded: a
/// pairing's few seconds, then the next `SpikeStats`.
pub const SECURE_WITHIN: Duration = Duration::from_secs(15);

/// Where an attachment's wait stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secure {
    /// No `SpikeStats` yet, and time left.
    Waiting,
    /// The bridge reported the link encrypted and bonded.
    Secured,
    /// The run fails, for the reason given.
    Failed(String),
}

/// One attachment's wait.
#[derive(Debug, Clone, Copy)]
pub struct SecureWait {
    deadline: Instant,
}

impl SecureWait {
    /// A wait from `attached`.
    #[must_use]
    pub fn new(attached: Instant) -> Self {
        Self { deadline: attached + SECURE_WITHIN }
    }

    /// Where the wait stands at `now`, given the attachment's latest `SpikeStats`.
    #[must_use]
    pub fn check(&self, stats: Option<&SpikeStats>, now: Instant) -> Secure {
        match stats {
            Some(stats) if stats.encrypted && stats.bonded => Secure::Secured,
            Some(stats) => Secure::Failed(format!(
                "the bridge reports the link encrypted {}, bonded {}; --expect-encrypted wants both",
                stats.encrypted, stats.bonded
            )),
            None if now >= self.deadline => Secure::Failed(format!(
                "no SpikeStats in {} s: the link is not encrypted and bonded, so the C6 \
                 forwards nothing",
                SECURE_WITHIN.as_secs()
            )),
            None => Secure::Waiting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(encrypted: bool, bonded: bool) -> SpikeStats {
        SpikeStats { encrypted, bonded, ..SpikeStats::default() }
    }

    #[test]
    fn secure_wait_secures_when_stats_report_encrypted_and_bonded() {
        let start = Instant::now();
        let wait = SecureWait::new(start);
        assert_eq!(wait.check(Some(&stats(true, true)), start), Secure::Secured);
    }

    #[test]
    fn secure_wait_fails_when_stats_report_link_unbonded() {
        let start = Instant::now();
        let wait = SecureWait::new(start);
        assert!(matches!(wait.check(Some(&stats(true, false)), start), Secure::Failed(_)));
        assert!(matches!(wait.check(Some(&stats(false, true)), start), Secure::Failed(_)));
    }

    #[test]
    fn secure_wait_fails_when_no_stats_arrive_in_time() {
        let start = Instant::now();
        let wait = SecureWait::new(start);
        assert_eq!(wait.check(None, start + SECURE_WITHIN / 2), Secure::Waiting);
        assert!(matches!(wait.check(None, start + SECURE_WITHIN), Secure::Failed(_)));
    }
}
