//! `HealthFeed` and `LocationFeed`, what the phone itself knows.

use wartui_core::feed::{LocationFix, Pushed};
use wartui_core::health::Health;
use wartui_core::runtime::now;
use wartui_ffi::{HealthFeed, LocationFeed, max_fix_accuracy_m};

#[test]
fn health_feed_gives_latest_reading_when_set_twice() {
    let feed = HealthFeed::new();
    feed.set(Health { battery_mv: Some(4_100), ..Health::default() });
    let latest = Health { battery_mv: Some(3_900), soc_temp_mc: Some(41_000), ..Health::default() };
    feed.set(latest);
    assert_eq!(feed.latest(), latest);
}

/// A fix taken now, reported to `accuracy_m`.
fn fix(accuracy_m: f64) -> LocationFix {
    LocationFix {
        lat: 10.0,
        lon: 20.0,
        alt_msl: None,
        accuracy_m: Some(accuracy_m),
        at_ms: now().unix_ms,
        mock: false,
    }
}

#[test]
fn location_feed_refuses_fix_when_accuracy_is_past_the_exported_limit() {
    let feed = LocationFeed::new();
    assert_eq!(feed.push(fix(max_fix_accuracy_m())), Pushed::Taken);
    assert_eq!(feed.push(fix(max_fix_accuracy_m() + 0.5)), Pushed::Coarse);
}
