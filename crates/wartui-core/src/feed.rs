//! Typed fixes from a platform's location services.
//!
//! A phone places itself through its own location services, which hand the app a position rather
//! than NMEA sentences, and may derive it from cell towers or Wi-Fi rather than satellites. A
//! [`FixFeed`] takes those fixes ([`FixFeed::push`]) and serves the latest to a
//! [`crate::position::Tier`], which stores the rows it places as
//! [`PositionSource::Platform`].
//!
//! **A typed fix ages by its own time, never by when it arrived.** A platform redelivers cached
//! locations, sometimes minutes old, so arrival says nothing about where the device is now. A fix
//! taken longer ago than the feed's limit is refused, one older than the fix held is refused
//! unless the host clock has stepped back behind the held one, and the chain ages the held fix by
//! the same clock. A fix's time is on the host's clock, so an adapter whose platform stamps
//! satellite time derives it from the fix's age instead ([`LocationFix::at_ms`]). **A fix cannot
//! have been taken after it arrived**, so one stamped ahead of the clock is held at its arrival
//! and ages from there: read as stamped, a negative age would keep it fresh until the clock caught
//! up. A held time ahead of the clock then means only that the clock stepped back. A receiver
//! differs: its sentences arrive as they are made, and it ages its fix by arrival
//! ([`crate::gps`]).
//!
//! **A fix run tolerates a wider gap than a receiver's** ([`DEFAULT_FEED_GAP`], against
//! [`crate::gps::FIX_GAP`]). A phone delivers about once a second with jitter, and 1.5 s would
//! break its run on an ordinary late fix. A reported loss ([`FixFeed::lost`]) ends the run at once.
//! An adapter asking for fixes less often widens the gap ([`FixFeed::with_max_gap`]), and keeps it
//! under the age limit.
//!
//! **A mock position is refused**, as test data: it is what `--lat`/`--lon` is to the terminal,
//! and a row it placed would be uploaded as a real sighting.
//!
//! **A coarse fix is refused here, not at export** ([`MAX_ACCURACY_M`]). Held by a tier, it would
//! shadow a good fix in a tier below; refused, the row takes the next tier's position or is stored
//! unpositioned.
//!
//! **`wanted` is advice.** The chain clears it while a tier above is placing the rows, so the
//! platform's adapter may slow or stop its location updates to save power. A fix pushed while
//! unwanted is still taken, and a feed in no chain stays wanted.
//!
//! Logging belongs here, each line only on a change, so resolving a position stays free of
//! effects.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use crate::position::{Fix, PositionSource, is_fresh};

/// The longest gap between taken fixes, by their own times, that still counts as one fix run
/// ([`FixFeed::fix_run`]).
///
/// At about 1 Hz this rides out two late fixes. An adapter that asks its platform for fixes less
/// often than about every two seconds sets a wider gap ([`FixFeed::with_max_gap`]), or its fixes
/// never form a run and a wait for a fix never ends.
pub const DEFAULT_FEED_GAP: Duration = Duration::from_secs(3);

/// The worst horizontal accuracy, in meters, a fix may report and still be taken.
///
/// The ceiling keeps network-derived fixes out. A position worked out from nearby Wi-Fi networks
/// is circular for a capture that places Wi-Fi networks, and a cell-tower fix is hundreds of
/// meters wide. Satellite fixes report well under it, even between buildings, while Wi-Fi-derived
/// ones commonly report more. The figure a platform reports is a radius it is about 68% sure of,
/// not a bound, which argues for the tighter limit. It is a judgment to revisit against real
/// phones, not a measurement.
pub const MAX_ACCURACY_M: f64 = 50.0;

/// One fix from a platform's location services.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocationFix {
    /// Degrees north.
    pub lat: f64,
    /// Degrees east.
    pub lon: f64,
    /// Meters above mean sea level, if known.
    pub alt_msl: Option<f64>,
    /// Horizontal accuracy in meters, if known. A fix without one is refused
    /// ([`Pushed::Coarse`]).
    pub accuracy_m: Option<f64>,
    /// When the fix was taken, in Unix milliseconds, on the same clock as the `now_ms` given to
    /// [`FixFeed::push`].
    ///
    /// A platform that stamps a fix with satellite time (Android does for GPS fixes) is not on
    /// that clock: its adapter derives this from the fix's age instead, the wall clock less how
    /// long ago the platform says the fix was taken. Otherwise a phone whose clock is ahead of
    /// satellite time by more than the age limit has every fix refused.
    ///
    /// A time ahead of `now_ms` is read as `now_ms`: a fix cannot have been taken after it
    /// arrived.
    pub at_ms: i64,
    /// Whether the platform marks this as a mock location.
    pub mock: bool,
}

/// What [`FixFeed::push`] did with a fix.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// The fix replaced the one held.
    Taken,
    /// Taken longer ago than the feed's limit, or before the fix held.
    Stale,
    /// The platform marked it a mock location.
    Mock,
    /// Its coordinates name no place on Earth.
    Invalid,
    /// Its accuracy is missing or worse than [`MAX_ACCURACY_M`].
    Coarse,
}

/// Why a platform's location services stopped giving fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedLoss {
    /// The user refused location permission.
    Denied,
    /// Location services are turned off.
    Disabled,
    /// No provider can place the device.
    Unavailable,
}

/// The feed's state, for a front end to report on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedView {
    /// When the held fix was taken, in Unix milliseconds, if one is held.
    pub last_fix_ms: Option<i64>,
    /// Why fixes stopped, until the next one is taken.
    pub loss: Option<FeedLoss>,
    /// Fixes taken.
    pub taken: u64,
    /// Fixes refused.
    pub refused: u64,
    /// Whether the chain wants fixes from this feed ([`FixFeed::wanted`]).
    pub wanted: bool,
}

#[derive(Debug)]
struct Inner {
    fix: Option<LocationFix>,
    /// The `at_ms` of the current run's first fix. `None` before the first and after a loss.
    run_since_ms: Option<i64>,
    loss: Option<FeedLoss>,
    taken: u64,
    refused: u64,
    max_age: Duration,
    max_gap: Duration,
    /// Whether fixes have been arriving since the start or the last loss, so the log says when
    /// they start.
    arriving: bool,
}

/// A handle on a platform's fixes, cheap to clone and safe to share.
#[derive(Debug, Clone)]
pub struct FixFeed {
    inner: Arc<Mutex<Inner>>,
    wanted: watch::Sender<bool>,
}

impl FixFeed {
    /// A feed that refuses fixes taken more than `max_age` ago. Its run gap is
    /// [`DEFAULT_FEED_GAP`], and it starts wanted.
    #[must_use]
    pub fn new(max_age: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                fix: None,
                run_since_ms: None,
                loss: None,
                taken: 0,
                refused: 0,
                max_age,
                max_gap: DEFAULT_FEED_GAP,
                arriving: false,
            })),
            wanted: watch::Sender::new(true),
        }
    }

    /// Use `max_gap` as the longest gap within one fix run.
    ///
    /// An adapter that asks its platform for fixes less often than about every two seconds sets a
    /// wider gap than [`DEFAULT_FEED_GAP`], or its fixes never form a run and a wait for a fix
    /// never ends. Keep it under the feed's `max_age`: a wider gap holds a run on a fix the chain
    /// no longer believes.
    #[must_use]
    pub fn with_max_gap(self, max_gap: Duration) -> Self {
        self.lock().max_gap = max_gap;
        self
    }

    /// Offer one fix, as of `now_ms`. A refused fix changes nothing but the refused count.
    pub fn push(&self, fix: LocationFix, now_ms: i64) -> Pushed {
        // A fix cannot have been taken after it arrived. Held as stamped, one ahead of the clock
        // would read as fresh until the clock caught up, and would make an adapter that stamps
        // slightly ahead look like a clock stepping back.
        let fix = LocationFix { at_ms: fix.at_ms.min(now_ms), ..fix };
        let mut inner = self.lock();
        // A held fix ahead of `now_ms` is the host clock stepping back, as `is_fresh` reads a
        // negative age. Refusing what is older than it would refuse every fix until the clock
        // caught up, while the held one served the rows as fresh, so an older fix replaces it.
        let held_ahead = inner.fix.is_some_and(|held| held.at_ms > now_ms);
        let older = inner.fix.is_some_and(|held| fix.at_ms < held.at_ms);
        let verdict = if fix.mock {
            Pushed::Mock
        } else if !is_a_place(fix.lat, fix.lon) {
            Pushed::Invalid
        } else if !is_precise(fix.accuracy_m) {
            Pushed::Coarse
        } else if !is_fresh(fix.at_ms, now_ms, inner.max_age) || (older && !held_ahead) {
            Pushed::Stale
        } else {
            Pushed::Taken
        };
        if verdict != Pushed::Taken {
            inner.refused += 1;
            return verdict;
        }
        let max_gap = inner.max_gap;
        // A fix older than the one it replaces came after the clock stepped back, and starts a run
        // of its own.
        let continues =
            !older && inner.fix.is_some_and(|held| is_fresh(held.at_ms, fix.at_ms, max_gap));
        if !continues {
            inner.run_since_ms = None;
        }
        inner.run_since_ms.get_or_insert(fix.at_ms);
        inner.fix = Some(fix);
        inner.loss = None;
        inner.taken += 1;
        if !inner.arriving {
            tracing::info!(at_ms = fix.at_ms, "location fixes arriving");
            inner.arriving = true;
        }
        Pushed::Taken
    }

    /// The platform stopped giving fixes, for `why`. Ends the fix run; the held fix stays, and the
    /// chain ages it out.
    pub fn lost(&self, why: FeedLoss) {
        let mut inner = self.lock();
        inner.run_since_ms = None;
        inner.arriving = false;
        if inner.loss != Some(why) {
            tracing::warn!(reason = ?why, "location fixes lost");
            inner.loss = Some(why);
        }
    }

    /// Whether the chain wants fixes from this feed: `false` while a tier above places the rows,
    /// so the adapter may slow or stop its location updates.
    #[must_use]
    pub fn wanted(&self) -> watch::Receiver<bool> {
        self.wanted.subscribe()
    }

    /// The held fix, as the chain stores it, stamped with its own time.
    #[must_use]
    pub fn latest(&self) -> Option<Fix> {
        let fix = self.lock().fix?;
        Some(Fix {
            lat: Some(fix.lat),
            lon: Some(fix.lon),
            alt: fix.alt_msl,
            accuracy: fix.accuracy_m,
            source: PositionSource::Platform,
            at_ms: Some(fix.at_ms),
        })
    }

    /// The fix run the feed holds as of `now_ms`, named by its first fix's own time: the latest
    /// fix was taken within the gap of `now_ms`, and no loss has ended the run. `None` otherwise.
    #[must_use]
    pub fn fix_run(&self, now_ms: i64) -> Option<i64> {
        let inner = self.lock();
        let since = inner.run_since_ms?;
        is_fresh(inner.fix?.at_ms, now_ms, inner.max_gap).then_some(since)
    }

    /// What to say about the feed.
    #[must_use]
    pub fn view(&self) -> FeedView {
        let inner = self.lock();
        FeedView {
            last_fix_ms: inner.fix.map(|fix| fix.at_ms),
            loss: inner.loss,
            taken: inner.taken,
            refused: inner.refused,
            wanted: *self.wanted.borrow(),
        }
    }

    /// Advise the adapter whether fixes are wanted. Only a change is logged.
    pub(crate) fn set_wanted(&self, wanted: bool) {
        let changed = self.wanted.send_if_modified(|now| std::mem::replace(now, wanted) != wanted);
        if !changed {
            return;
        }
        if wanted {
            tracing::info!("location fixes wanted again");
        } else {
            tracing::info!("location fixes not wanted: a tier above places the rows");
        }
    }

    /// A poisoned mutex means a previous holder panicked mid-update; what it left is still
    /// readable and better than taking the capture down over a position.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether `accuracy_m` is a reported accuracy no worse than [`MAX_ACCURACY_M`]. A negative one is
/// how iOS reports an invalid fix.
fn is_precise(accuracy_m: Option<f64>) -> bool {
    accuracy_m.is_some_and(|m| m.is_finite() && (0.0..=MAX_ACCURACY_M).contains(&m))
}

/// Whether `lat` and `lon` are finite and on the globe.
fn is_a_place(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_AGE: Duration = Duration::from_secs(5);

    /// A fix at the tests' synthetic place, taken at `at_ms`.
    fn fix_at(at_ms: i64) -> LocationFix {
        LocationFix {
            lat: 37.7749,
            lon: -122.4194,
            alt_msl: Some(16.0),
            accuracy_m: Some(8.0),
            at_ms,
            mock: false,
        }
    }

    #[test]
    fn fix_feed_takes_fix_when_its_time_is_recent() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(9_000), 10_000), Pushed::Taken);
        let fix = feed.latest().expect("a fix");
        assert_eq!(fix.lat, Some(37.7749));
        assert_eq!(fix.lon, Some(-122.4194));
        assert_eq!(fix.alt, Some(16.0));
        assert_eq!(fix.accuracy, Some(8.0));
        assert_eq!(fix.source, PositionSource::Platform);
        assert_eq!(fix.at_ms, Some(9_000), "the fix's own time, not its arrival");
        assert_eq!(feed.view().taken, 1);
    }

    #[test]
    fn fix_feed_rejects_fix_when_its_time_is_past_max_age() {
        // A platform redelivers its cached location: arriving now says nothing about now.
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(4_999), 10_000), Pushed::Stale);
        assert_eq!(feed.latest(), None);
        assert_eq!(feed.view().refused, 1);
    }

    #[test]
    fn fix_feed_rejects_fix_when_it_is_mock() {
        let feed = FixFeed::new(MAX_AGE);
        let mock = LocationFix { mock: true, ..fix_at(10_000) };
        assert_eq!(feed.push(mock, 10_000), Pushed::Mock);
        assert_eq!(feed.latest(), None);
    }

    #[test]
    fn fix_feed_rejects_fix_when_coordinates_are_not_a_place() {
        let feed = FixFeed::new(MAX_AGE);
        for (lat, lon) in [(f64::NAN, -122.4194), (91.0, -122.4194), (37.7749, 181.0)] {
            let fix = LocationFix { lat, lon, ..fix_at(10_000) };
            assert_eq!(feed.push(fix, 10_000), Pushed::Invalid, "{lat}, {lon}");
        }
        assert_eq!(feed.latest(), None);
        assert_eq!(feed.view().refused, 3);
    }

    #[test]
    fn fix_feed_rejects_fix_when_its_accuracy_is_worse_than_the_limit() {
        let feed = FixFeed::new(MAX_AGE);
        let coarse = LocationFix { accuracy_m: Some(50.1), ..fix_at(10_000) };
        assert_eq!(feed.push(coarse, 10_000), Pushed::Coarse);
        assert_eq!(feed.latest(), None);
        assert_eq!(feed.view().refused, 1);
    }

    #[test]
    fn fix_feed_takes_fix_when_its_accuracy_is_exactly_the_limit() {
        let feed = FixFeed::new(MAX_AGE);
        let edge = LocationFix { accuracy_m: Some(MAX_ACCURACY_M), ..fix_at(10_000) };
        assert_eq!(feed.push(edge, 10_000), Pushed::Taken);
        assert_eq!(feed.latest().and_then(|fix| fix.accuracy), Some(50.0));
    }

    #[test]
    fn fix_feed_rejects_fix_when_it_reports_no_accuracy() {
        // A negative accuracy is how iOS reports an invalid fix.
        let feed = FixFeed::new(MAX_AGE);
        for accuracy_m in [None, Some(f64::NAN), Some(-1.0)] {
            let fix = LocationFix { accuracy_m, ..fix_at(10_000) };
            assert_eq!(feed.push(fix, 10_000), Pushed::Coarse, "{accuracy_m:?}");
        }
        assert_eq!(feed.latest(), None);
        assert_eq!(feed.view().refused, 3);
    }

    #[test]
    fn fix_feed_rejects_fix_when_older_than_the_one_held() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(10_000), 10_000), Pushed::Taken);
        let older = LocationFix { lat: 37.0, ..fix_at(9_500) };
        assert_eq!(feed.push(older, 10_100), Pushed::Stale);
        assert_eq!(feed.latest().and_then(|fix| fix.lat), Some(37.7749));
    }

    #[test]
    fn fix_feed_takes_fix_when_host_clock_steps_back_past_the_one_held() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(100_000), 100_000), Pushed::Taken);
        // An NTP correction steps the clock back a minute: the held fix is now ahead of it.
        let after = LocationFix { lat: 37.0, ..fix_at(40_000) };
        assert_eq!(feed.push(after, 40_000), Pushed::Taken);
        assert_eq!(feed.latest().and_then(|fix| fix.at_ms), Some(40_000));
        assert_eq!(feed.fix_run(40_000), Some(40_000), "the fix starts a run of its own");
        // Behind the held one again once it is not ahead of the clock.
        let older = LocationFix { lat: 36.0, ..fix_at(39_500) };
        assert_eq!(feed.push(older, 40_100), Pushed::Stale);
    }

    #[test]
    fn fix_feed_keeps_fix_run_when_fixes_are_stamped_ahead_of_the_clock() {
        // An adapter whose stamps run ahead of the clock it passes must still hold a run, or a
        // wait for a fix would never end. Each fix is held at its arrival, which names the run.
        let feed = FixFeed::new(MAX_AGE);
        for at_ms in [10_500, 11_500, 12_500] {
            assert_eq!(feed.push(fix_at(at_ms), at_ms - 500), Pushed::Taken);
        }
        assert_eq!(feed.fix_run(12_000), Some(10_000));
    }

    #[test]
    fn fix_feed_ages_fix_from_its_arrival_when_stamped_ahead_of_the_clock() {
        // A phone whose clock runs two minutes behind satellite time, with an adapter passing the
        // satellite stamp.
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(130_000), 10_000), Pushed::Taken);
        assert_eq!(feed.latest().and_then(|fix| fix.at_ms), Some(10_000), "held at its arrival");
        assert_eq!(feed.view().last_fix_ms, Some(10_000));
        assert_eq!(feed.fix_run(13_000), Some(10_000));
        assert_eq!(feed.fix_run(13_001), None, "the run lapses a gap after arrival");
    }

    #[test]
    fn fix_feed_rejects_late_older_fix_when_adapter_stamps_ahead_of_the_clock() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(12_500), 12_000), Pushed::Taken);
        // Taken before the held one, which is not ahead of the clock: no step back, just late.
        let late = LocationFix { lat: 37.0, ..fix_at(11_800) };
        assert_eq!(feed.push(late, 12_100), Pushed::Stale);
        assert_eq!(feed.latest().and_then(|fix| fix.lat), Some(37.7749));
        assert_eq!(feed.fix_run(12_100), Some(12_000), "the run is undisturbed");
    }

    #[test]
    fn fix_feed_names_fix_run_by_its_first_fix_when_fixes_arrive_within_the_gap() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.fix_run(10_000), None);
        for at_ms in [10_000, 11_000, 14_000] {
            assert_eq!(feed.push(fix_at(at_ms), at_ms), Pushed::Taken);
        }
        assert_eq!(feed.fix_run(14_000), Some(10_000));
    }

    #[test]
    fn fix_feed_starts_new_fix_run_when_fixes_are_further_apart_than_the_gap() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(10_000), 10_000), Pushed::Taken);
        assert_eq!(feed.push(fix_at(13_001), 13_001), Pushed::Taken);
        assert_eq!(feed.fix_run(13_001), Some(13_001));
    }

    #[test]
    fn fix_feed_holds_no_fix_run_when_last_fix_is_past_the_gap() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(10_000), 10_000), Pushed::Taken);
        assert_eq!(feed.fix_run(13_000), Some(10_000), "exactly at the gap");
        assert_eq!(feed.fix_run(13_001), None);
    }

    #[test]
    fn fix_feed_ends_fix_run_but_keeps_fix_when_lost() {
        let feed = FixFeed::new(MAX_AGE);
        assert_eq!(feed.push(fix_at(10_000), 10_000), Pushed::Taken);
        feed.lost(FeedLoss::Disabled);
        assert_eq!(feed.fix_run(10_000), None);
        assert!(feed.latest().is_some(), "the chain ages the held fix out");
        assert_eq!(feed.view().loss, Some(FeedLoss::Disabled));
        // The next fix starts a run of its own, even within the gap.
        assert_eq!(feed.push(fix_at(11_000), 11_000), Pushed::Taken);
        assert_eq!(feed.fix_run(11_000), Some(11_000));
    }

    #[test]
    fn fix_feed_clears_loss_when_a_fix_arrives() {
        let feed = FixFeed::new(MAX_AGE);
        feed.lost(FeedLoss::Denied);
        assert_eq!(feed.view().loss, Some(FeedLoss::Denied));
        assert_eq!(feed.push(fix_at(10_000), 10_000), Pushed::Taken);
        assert_eq!(feed.view().loss, None);
    }

    #[test]
    fn fix_feed_is_wanted_when_new() {
        let feed = FixFeed::new(MAX_AGE);
        assert!(*feed.wanted().borrow());
        assert!(feed.view().wanted);
    }
}
