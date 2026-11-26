//! `HealthFeed` and `LocationFeed`, what the phone itself knows.

use wartui_core::health::Health;
use wartui_ffi::HealthFeed;

#[test]
fn health_feed_gives_latest_reading_when_set_twice() {
    let feed = HealthFeed::new();
    feed.set(Health { battery_mv: Some(4_100), ..Health::default() });
    let latest = Health { battery_mv: Some(3_900), soc_temp_mc: Some(41_000), ..Health::default() };
    feed.set(latest);
    assert_eq!(feed.latest(), latest);
}
