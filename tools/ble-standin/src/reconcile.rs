//! The run's seq gaps against the P4's own counts of what it dropped.
//!
//! `SpikeStats.evicted` and `refused` count since the P4 booted, so the run reads them as the
//! last `SpikeStats` less the first. The generator sends only while a host owns the queue and the
//! queue starts empty at each attachment, so the first report, 2 s in, misses nothing. Both count
//! every frame the host rings dropped, node records and reports included, so they bound the
//! `SpikeLoad` losses from above.

use serde::Serialize;
use wartui_proto::spike::SpikeStats;

use crate::attachment::AttachmentSummary;

/// The run's losses and the P4's explanation for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Reconcile {
    /// Seqs skipped, over every attachment.
    pub missing: u64,
    /// Ranges they were skipped in.
    pub gap_ranges: usize,
    /// Frames the P4's host rings evicted during the run, or `None` without two reports.
    pub evicted: Option<u32>,
    /// Frames they refused.
    pub refused: Option<u32>,
    /// Skipped seqs neither count explains: lost between the P4's queue and here.
    pub unexplained: Option<i64>,
}

impl Reconcile {
    /// Reconcile `attachments`, in the order they ran.
    #[must_use]
    pub fn of(attachments: &[AttachmentSummary]) -> Self {
        let missing = attachments.iter().map(|a| a.spike.missing).sum::<u64>();
        let gap_ranges = attachments.iter().map(|a| a.spike.gaps.len()).sum();
        let first = attachments.iter().find_map(|a| a.first_stats);
        let last = attachments.iter().rev().find_map(|a| a.last_stats);
        let delta = |of: fn(&SpikeStats) -> u32| {
            first.zip(last).map(|(first, last)| of(&last).wrapping_sub(of(&first)))
        };
        let evicted = delta(|s| s.evicted);
        let refused = delta(|s| s.refused);
        let unexplained =
            evicted.zip(refused).map(|(e, r)| missing as i64 - i64::from(e) - i64::from(r));
        Self { missing, gap_ranges, evicted, refused, unexplained }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::attachment::Attachment;

    fn summary(
        missing: u64,
        first: Option<(u32, u32)>,
        last: Option<(u32, u32)>,
    ) -> AttachmentSummary {
        let stats = |(evicted, refused)| SpikeStats { evicted, refused, ..Default::default() };
        let mut summary = Attachment::new(Instant::now()).summary(Instant::now());
        summary.spike.missing = missing;
        summary.first_stats = first.map(stats);
        summary.last_stats = last.map(stats);
        summary
    }

    #[test]
    fn reconcile_explains_gaps_when_evictions_match() {
        let run =
            [summary(4, Some((10, 0)), Some((13, 0))), summary(2, Some((13, 0)), Some((15, 1)))];
        let reconcile = Reconcile::of(&run);
        assert_eq!(reconcile.missing, 6);
        assert_eq!((reconcile.evicted, reconcile.refused), (Some(5), Some(1)));
        assert_eq!(reconcile.unexplained, Some(0));
    }

    #[test]
    fn reconcile_reports_unexplained_when_gaps_exceed_drops() {
        let reconcile = Reconcile::of(&[summary(9, Some((0, 0)), Some((2, 0)))]);
        assert_eq!(reconcile.unexplained, Some(7));
    }

    #[test]
    fn reconcile_leaves_drops_unknown_when_no_stats_arrived() {
        let reconcile = Reconcile::of(&[summary(3, None, None)]);
        assert_eq!((reconcile.evicted, reconcile.unexplained), (None, None));
    }
}
