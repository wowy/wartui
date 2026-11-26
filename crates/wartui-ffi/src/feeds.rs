//! What the phone itself knows: where it is, and how it is.
//!
//! [`LocationFeed`] is `wartui_core::feed::FixFeed` over the platform's location services. The
//! feed's rules are its own (`feed.rs` `//!`): a fix ages by when it was taken, on the host's
//! clock, and a mock or coarse fix is refused. [`LocationFeed::push`] stamps arrival with the
//! clock the engine reads, so the phone gives only when the fix was taken. The feed's age limit is
//! `wartui_core::position::DEFAULT_MAX_AGE`, the position chain's.
//!
//! [`HealthFeed`] holds the host's latest health reading, which every capture row records.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use wartui_core::feed::{FeedLoss, FeedView, FixFeed, LocationFix, MAX_ACCURACY_M, Pushed};
use wartui_core::health::{Health, HealthReader};
use wartui_core::position::DEFAULT_MAX_AGE;
use wartui_core::runtime::now;

/// One fix from the platform's location services.
#[uniffi::remote(Record)]
pub struct LocationFix {
    /// Degrees north.
    pub lat: f64,
    /// Degrees east.
    pub lon: f64,
    /// Meters above mean sea level, if known.
    pub alt_msl: Option<f64>,
    /// Horizontal accuracy in meters. A fix without one, or worse than
    /// [`max_fix_accuracy_m`], is refused.
    pub accuracy_m: Option<f64>,
    /// When the fix was taken, in Unix milliseconds on this device's clock. Where the platform
    /// stamps satellite time (Android does for GPS fixes), give the wall clock less the fix's age
    /// instead.
    pub at_ms: i64,
    /// Whether the platform marks it a mock location, which is refused.
    pub mock: bool,
}

/// What [`LocationFeed::push`] did with a fix.
#[uniffi::remote(Enum)]
pub enum Pushed {
    /// The fix replaced the one held.
    Taken,
    /// Taken longer ago than the feed's limit, or before the fix held.
    Stale,
    /// The platform marked it a mock location.
    Mock,
    /// Its coordinates name no place on Earth.
    Invalid,
    /// Its accuracy is missing or worse than [`max_fix_accuracy_m`].
    Coarse,
}

/// Why the platform's location services stopped giving fixes.
#[uniffi::remote(Enum)]
pub enum FeedLoss {
    /// The user refused location permission.
    Denied,
    /// Location services are turned off.
    Disabled,
    /// No provider can place the device.
    Unavailable,
}

/// The feed's state.
#[uniffi::remote(Record)]
pub struct FeedView {
    /// When the held fix was taken, in Unix milliseconds, if one is held.
    pub last_fix_ms: Option<i64>,
    /// Why fixes stopped, until the next one is taken.
    pub loss: Option<FeedLoss>,
    /// Fixes taken.
    pub taken: u64,
    /// Fixes refused.
    pub refused: u64,
    /// Whether the capture wants fixes now.
    pub wanted: bool,
}

/// The worst horizontal accuracy a fix may report and still be taken, in meters
/// (`wartui_core::feed::MAX_ACCURACY_M`).
#[uniffi::export]
#[must_use]
pub fn max_fix_accuracy_m() -> f64 {
    MAX_ACCURACY_M
}

/// The platform's location services, as a capture's position tier.
#[derive(Debug, uniffi::Object)]
pub struct LocationFeed {
    feed: FixFeed,
}

#[uniffi::export]
impl LocationFeed {
    /// A feed for a platform delivering about once a second.
    #[uniffi::constructor]
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { feed: FixFeed::new(DEFAULT_MAX_AGE) })
    }

    /// A feed for a platform asked for fixes less often: `max_gap_ms` is the longest gap between
    /// fixes that still counts as one run. Keep it under 5 s, the age a fix is believed to.
    #[uniffi::constructor]
    #[must_use]
    pub fn with_max_gap(max_gap_ms: u32) -> Arc<Self> {
        let gap = Duration::from_millis(u64::from(max_gap_ms));
        Arc::new(Self { feed: FixFeed::new(DEFAULT_MAX_AGE).with_max_gap(gap) })
    }

    /// One fix from the platform, as it arrives. The answer says whether it was taken, and why
    /// not: a mock, coarse or stale fix is refused.
    pub fn push(&self, fix: LocationFix) -> Pushed {
        self.feed.push(fix, now().unix_ms)
    }

    /// The platform stopped giving fixes.
    pub fn lost(&self, why: FeedLoss) {
        self.feed.lost(why);
    }

    /// Whether the capture wants fixes now. Advice: the platform's adapter may ease off while it
    /// is `false`.
    #[must_use]
    pub fn wanted(&self) -> bool {
        *self.feed.wanted().borrow()
    }

    /// Waits until `wanted` differs from `was`, and returns it.
    pub async fn next_wanted(&self, was: bool) -> bool {
        let mut wanted = self.feed.wanted();
        match wanted.wait_for(|now| *now != was).await {
            Ok(now) => *now,
            // The feed holds the sender as long as this object lives.
            Err(_) => was,
        }
    }

    /// The feed's state.
    #[must_use]
    pub fn state(&self) -> FeedView {
        self.feed.view()
    }
}

impl LocationFeed {
    /// The feed, for a capture's position chain.
    pub(crate) fn feed(&self) -> FixFeed {
        self.feed.clone()
    }
}

/// The host's health, as each capture row records it.
#[uniffi::remote(Record)]
pub struct Health {
    /// The Raspberry Pi firmware's `get_throttled` word. `None` off a Pi.
    pub throttled: Option<u32>,
    /// The SoC's temperature, in thousandths of a degree Celsius.
    pub soc_temp_mc: Option<i32>,
    /// Battery voltage in millivolts.
    pub battery_mv: Option<i32>,
    /// Battery current in milliamps, signed as the platform reports it.
    pub battery_ma: Option<i32>,
}

/// The host's latest health reading.
#[derive(Debug, uniffi::Object)]
pub struct HealthFeed {
    latest: Arc<Mutex<Health>>,
}

#[uniffi::export]
impl HealthFeed {
    /// A feed holding no reading: every value unread.
    #[uniffi::constructor]
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { latest: Arc::default() })
    }

    /// The host's health now; each capture row records the latest.
    pub fn set(&self, health: Health) {
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner) = health;
    }
}

impl HealthFeed {
    /// What a capture reads the host's health through: a copy of the latest reading.
    pub(crate) fn reader(&self) -> HealthReader {
        let latest = Arc::clone(&self.latest);
        Box::new(move || *latest.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The latest reading.
    #[must_use]
    pub fn latest(&self) -> Health {
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_feed_reader_reads_latest_when_set_after_it_was_made() {
        let feed = HealthFeed::new();
        let mut read = feed.reader();
        let reading = Health { battery_mv: Some(3_900), ..Health::default() };
        feed.set(reading);
        assert_eq!(read(), reading);
    }

    /// A receiver's fix, mid-ocean, as one `GGA` sentence.
    fn gga() -> Vec<u8> {
        let body = "GPGGA,123519.00,1000.000,N,04000.000,W,1,08,0.9,10.0,M,0.0,M,,";
        let sum = body.bytes().fold(0u8, |sum, byte| sum ^ byte);
        format!("${body}*{sum:02X}").into_bytes()
    }

    #[tokio::test]
    async fn location_feed_wakes_next_wanted_when_wanted_changes() {
        let location = LocationFeed::new();
        let bridge = wartui_core::gps::Gps::bridged(DEFAULT_MAX_AGE);
        let sources = wartui_app::position::Sources {
            bridge: Some(bridge.clone()),
            platform: Some(location.feed()),
            ..Default::default()
        };
        let chain = sources.chain(DEFAULT_MAX_AGE);
        assert!(location.wanted());
        let waiting = {
            let location = Arc::clone(&location);
            tokio::spawn(async move { location.next_wanted(true).await })
        };
        // The bridge's receiver holds a fix run, so the phone's fixes are not wanted.
        let now_ms = now().unix_ms;
        bridge.feed(&gga(), now_ms);
        chain.stand_down(now_ms, true);
        let woken = tokio::time::timeout(Duration::from_secs(5), waiting).await;
        assert!(!woken.expect("woken in time").expect("no panic"));
        assert!(!location.state().wanted);
    }
}
