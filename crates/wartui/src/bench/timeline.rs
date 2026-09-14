//! The measured window cut into slices, so a long run shows *when* it changed.
//!
//! A total hides a card that slows down twenty minutes in — when its write cache fills,
//! when the database outgrows SQLite's page cache and index pages start coming back off
//! the card, or when the board gets hot. Each slice is the difference between two marks
//! taken at sample times, so it holds only what happened inside it.
//!
//! A slice's device counters are what reached the card during it, which is not quite
//! what the store wrote during it: the kernel holds dirty pages for up to about thirty
//! seconds before writing them back. Nothing is synced between slices, because a sync a
//! minute would change the very writeback being measured. A trend survives that smear;
//! one slice's device figure read on its own does not.

use std::time::Duration;

use super::io::{DeviceIo, ProcessIo};

/// The run's cumulative counters at one moment.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mark {
    /// Time since the measured window opened.
    pub at: Duration,
    /// Observations the engine has heard.
    pub observations: u64,
    /// Rows the store has written.
    pub written: u64,
    /// Rows the store has dropped.
    pub dropped: u64,
    /// Node-windows that heard nothing.
    pub idle_windows: u64,
    /// This process's I/O, where the kernel keeps it.
    pub process: Option<ProcessIo>,
    /// The card's I/O, where the kernel keeps it.
    pub device: Option<DeviceIo>,
}

/// What happened between two marks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slice {
    /// When the slice opened, since the measured window did.
    pub start: Duration,
    /// When it closed.
    pub end: Duration,
    /// Observations heard inside it.
    pub observations: u64,
    /// Rows written inside it.
    pub written: u64,
    /// Rows dropped inside it.
    pub dropped: u64,
    /// Node-windows inside it that heard nothing.
    pub idle_windows: u64,
    /// Process I/O inside it, if both marks had it.
    pub process: Option<ProcessIo>,
    /// Device I/O inside it, if both marks had it.
    pub device: Option<DeviceIo>,
}

impl Slice {
    fn between(from: &Mark, to: &Mark) -> Self {
        Self {
            start: from.at,
            end: to.at,
            observations: to.observations.saturating_sub(from.observations),
            written: to.written.saturating_sub(from.written),
            dropped: to.dropped.saturating_sub(from.dropped),
            idle_windows: to.idle_windows.saturating_sub(from.idle_windows),
            process: to.process.zip(from.process).map(|(to, from)| to.since(from)),
            device: to.device.zip(from.device).map(|(to, from)| to.since(from)),
        }
    }

    /// How long the slice ran, which is a little over the interval: it closes at the
    /// first sample past its boundary, not on it.
    pub fn seconds(&self) -> f64 {
        (self.end - self.start).as_secs_f64()
    }
}

/// Collects slices from a stream of marks.
#[derive(Debug)]
pub struct Timeline {
    every: Duration,
    next: Duration,
    open: Mark,
    slices: Vec<Slice>,
}

impl Timeline {
    /// Start cutting at `start`, a slice every `every`. `every` must not be zero.
    pub fn new(every: Duration, start: Mark) -> Self {
        Self { every, next: start.at + every, open: start, slices: Vec::new() }
    }

    /// Offer a sample. The slice closes once a sample reaches its boundary; a sample
    /// that arrives past several boundaries closes one slice, not an empty one each.
    pub fn sample(&mut self, mark: Mark) {
        if mark.at < self.next {
            return;
        }
        self.close(mark);
        while self.next <= mark.at {
            self.next += self.every;
        }
    }

    /// End the run at `mark`, keeping a short last slice unless it would be empty.
    pub fn finish(mut self, mark: Mark) -> Vec<Slice> {
        if mark.at > self.open.at {
            self.close(mark);
        }
        self.slices
    }

    fn close(&mut self, mark: Mark) {
        self.slices.push(Slice::between(&self.open, &mark));
        self.open = mark;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DeviceIo, Mark, Timeline};

    fn at(seconds: u64) -> Mark {
        Mark {
            at: Duration::from_secs(seconds),
            observations: seconds * 10,
            written: seconds * 20,
            ..Mark::default()
        }
    }

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn a_slice_closes_at_each_boundary_and_the_short_tail_is_kept() {
        let mut timeline = Timeline::new(MINUTE, at(0));
        for seconds in (2..=130).step_by(2) {
            timeline.sample(at(seconds));
        }
        let slices = timeline.finish(at(131));

        let ends: Vec<u64> = slices.iter().map(|s| s.end.as_secs()).collect();
        assert_eq!(ends, [60, 120, 131]);
        let observations: Vec<u64> = slices.iter().map(|s| s.observations).collect();
        assert_eq!(observations, [600, 600, 110], "each slice holds only its own");
        assert_eq!(slices[2].written, 220);
    }

    #[test]
    fn a_run_ending_on_a_boundary_has_no_empty_last_slice() {
        let mut timeline = Timeline::new(MINUTE, at(0));
        timeline.sample(at(60));
        assert_eq!(timeline.finish(at(60)).len(), 1);
    }

    #[test]
    fn a_sample_late_past_several_boundaries_closes_one_slice_and_stays_on_the_grid() {
        // A stalled sampler is a slow run's symptom, not a reason to invent slices.
        let mut timeline = Timeline::new(MINUTE, at(0));
        timeline.sample(at(190));
        timeline.sample(at(230));
        timeline.sample(at(240));
        let slices = timeline.finish(at(240));

        let bounds: Vec<(u64, u64)> =
            slices.iter().map(|s| (s.start.as_secs(), s.end.as_secs())).collect();
        assert_eq!(bounds, [(0, 190), (190, 240)]);
    }

    #[test]
    fn device_counters_in_a_slice_need_both_ends() {
        let device = DeviceIo { writes: 5, sectors: 40, write_ms: 1, flushes: None };
        let mut timeline = Timeline::new(MINUTE, at(0));
        timeline.sample(Mark { device: Some(device), ..at(60) });
        timeline.sample(Mark {
            device: Some(DeviceIo { writes: 9, sectors: 100, ..device }),
            ..at(120)
        });
        let slices = timeline.finish(at(120));

        assert_eq!(slices[0].device, None, "the first mark had no counters");
        assert_eq!(slices[1].device.map(|d| (d.writes, d.sectors)), Some((4, 60)));
    }
}
