//! `GetStatus` round trips, one at a time.
//!
//! A `Status` carries no id, so only one `GetStatus` is ever out: the next `Status` answers it.
//! A `Status` is a priority frame, so the time measures the link both ways, not the bulk ring.
//! A [`StatusAsk`], the bridge counters' snapshot, waits for a timed ask to finish and holds the
//! next one back until its own is answered, so each `Status` answers the one ask out.

use std::time::{Duration, Instant};

use serde::Serialize;

/// How long a `GetStatus` waits before it counts as lost.
pub const RTT_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between an answer and the next `GetStatus`.
pub const RTT_GAP: Duration = Duration::from_millis(250);

/// The round trips asked for, measured as they complete.
#[derive(Debug, Clone)]
pub struct Rtt {
    left: u32,
    out: Option<Instant>,
    next_at: Option<Instant>,
    samples: Vec<Duration>,
    lost: u32,
}

/// What the round trips came to, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RttSummary {
    /// Answers timed.
    pub answered: usize,
    /// Asks unanswered within [`RTT_TIMEOUT`], or when the link dropped.
    pub lost: u32,
    /// The median.
    pub p50_ms: f64,
    /// The 99th percentile, nearest rank.
    pub p99_ms: f64,
    /// The slowest.
    pub max_ms: f64,
}

impl Rtt {
    /// `count` round trips to run.
    #[must_use]
    pub const fn new(count: u32) -> Self {
        Self { left: count, out: None, next_at: None, samples: Vec::new(), lost: 0 }
    }

    /// Whether a `GetStatus` should be sent at `now`. Call [`Rtt::sent`] when it is.
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        self.left > 0 && self.out.is_none() && self.next_at.is_none_or(|at| now >= at)
    }

    /// Whether an ask is waiting for its answer.
    #[must_use]
    pub const fn is_out(&self) -> bool {
        self.out.is_some()
    }

    /// A `GetStatus` went out at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.left = self.left.saturating_sub(1);
        self.out = Some(now);
    }

    /// A `Status` arrived at `now`. Returns the round trip it closed, if one was out.
    pub fn answered(&mut self, now: Instant) -> Option<Duration> {
        let sent = self.out.take()?;
        let rtt = now.saturating_duration_since(sent);
        self.samples.push(rtt);
        self.next_at = Some(now + RTT_GAP);
        Some(rtt)
    }

    /// Give up on an ask out longer than [`RTT_TIMEOUT`] at `now`.
    pub fn expire(&mut self, now: Instant) {
        if self.out.is_some_and(|sent| now.saturating_duration_since(sent) >= RTT_TIMEOUT) {
            self.drop_out(now);
        }
    }

    /// The link dropped: whatever is out is lost.
    pub fn drop_out(&mut self, now: Instant) {
        if self.out.take().is_some() {
            self.lost += 1;
            self.next_at = Some(now + RTT_GAP);
        }
    }

    /// The percentiles so far, or `None` before any answer or loss.
    #[must_use]
    pub fn summary(&self) -> Option<RttSummary> {
        if self.samples.is_empty() && self.lost == 0 {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let ms = |d: Option<&Duration>| d.map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        Some(RttSummary {
            answered: sorted.len(),
            lost: self.lost,
            p50_ms: ms(nearest_rank(&sorted, 50)),
            p99_ms: ms(nearest_rank(&sorted, 99)),
            max_ms: ms(sorted.last()),
        })
    }
}

/// A `GetStatus` for the bridge counters' snapshot, outside the timed round trips.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusAsk {
    wanted: bool,
    out: Option<Instant>,
}

impl StatusAsk {
    /// Ask once the timed ask, if any, is answered.
    pub fn want(&mut self) {
        self.wanted = true;
    }

    /// Whether to send it now, with `rtt_out` saying whether a timed ask is out.
    #[must_use]
    pub const fn due(&self, rtt_out: bool) -> bool {
        self.wanted && self.out.is_none() && !rtt_out
    }

    /// It went out at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.wanted = false;
        self.out = Some(now);
    }

    /// Whether it is wanted or out, which holds the timed asks back.
    #[must_use]
    pub const fn pending(&self) -> bool {
        self.wanted || self.out.is_some()
    }

    /// A `Status` arrived. Returns whether it answered this ask rather than a timed one.
    pub fn answered(&mut self) -> bool {
        self.out.take().is_some()
    }

    /// Give up on an ask out longer than [`RTT_TIMEOUT`] at `now`.
    pub fn expire(&mut self, now: Instant) {
        if self.out.is_some_and(|sent| now.saturating_duration_since(sent) >= RTT_TIMEOUT) {
            self.out = None;
        }
    }
}

/// The `p`th percentile of `sorted` by nearest rank.
fn nearest_rank(sorted: &[Duration], p: usize) -> Option<&Duration> {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn rtt_reports_percentiles_when_hundred_answers_timed() {
        let start = Instant::now();
        let mut rtt = Rtt::new(100);
        let mut now = start;
        for n in 1..=100 {
            assert!(rtt.due(now), "ask {n}");
            rtt.sent(now);
            now += ms(n);
            assert_eq!(rtt.answered(now), Some(ms(n)));
            now += RTT_GAP;
        }
        assert!(!rtt.due(now));
        let summary = rtt.summary().unwrap();
        assert_eq!(summary.answered, 100);
        assert!((summary.p50_ms - 50.0).abs() < 1e-6, "{summary:?}");
        assert!((summary.p99_ms - 99.0).abs() < 1e-6, "{summary:?}");
        assert!((summary.max_ms - 100.0).abs() < 1e-6, "{summary:?}");
    }

    #[test]
    fn rtt_waits_for_answer_when_ask_is_out() {
        let now = Instant::now();
        let mut rtt = Rtt::new(3);
        rtt.sent(now);
        assert!(!rtt.due(now + ms(1_000)));
        rtt.answered(now + ms(10));
        assert!(!rtt.due(now + ms(10)), "the gap holds the next ask");
        assert!(rtt.due(now + ms(10) + RTT_GAP));
    }

    #[test]
    fn rtt_ignores_status_when_nothing_is_out() {
        let mut rtt = Rtt::new(1);
        assert_eq!(rtt.answered(Instant::now()), None);
        assert_eq!(rtt.summary(), None);
    }

    #[test]
    fn status_ask_waits_when_timed_ask_is_out() {
        let now = Instant::now();
        let mut rtt = Rtt::new(5);
        let mut ask = StatusAsk::default();
        rtt.sent(now);
        ask.want();
        assert!(!ask.due(rtt.is_out()));
        // The first `Status` answers the timed ask.
        assert!(!ask.answered());
        rtt.answered(now + ms(30));
        assert!(ask.due(rtt.is_out()));
        ask.sent(now + ms(40));
        assert!(ask.pending(), "holds the next timed ask back");
        assert!(ask.answered());
        assert!(!ask.pending());
    }

    #[test]
    fn status_ask_gives_up_when_answer_times_out() {
        let now = Instant::now();
        let mut ask = StatusAsk::default();
        ask.want();
        ask.sent(now);
        ask.expire(now + RTT_TIMEOUT);
        assert!(!ask.pending());
        assert!(!ask.answered());
    }

    #[test]
    fn rtt_counts_lost_when_answer_times_out() {
        let now = Instant::now();
        let mut rtt = Rtt::new(2);
        rtt.sent(now);
        rtt.expire(now + ms(100));
        assert!(!rtt.due(now + ms(100)));
        rtt.expire(now + RTT_TIMEOUT);
        let summary = rtt.summary().unwrap();
        assert_eq!((summary.answered, summary.lost), (0, 1));
        assert!(rtt.due(now + RTT_TIMEOUT + RTT_GAP));
    }
}
