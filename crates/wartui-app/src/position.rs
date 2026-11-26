//! Where a front end's rows are placed: the position chain's tiers, and whether the scan waits
//! for a fix. Built here once, so a desktop and a phone order their sources by one rule.
//!
//! The order is the operator's word first, then the best receiver, then the guesses:
//!
//! 1. **A receiver the operator named**, since naming it is an instruction.
//! 2. **The bridge's receiver**, which travels with the fleet.
//! 3. **The platform's location services**, standing down while a tier above holds a fix run
//!    ([`StandDown::WhileAboveFixed`]). A phone's location may be network-derived, so it yields
//!    to the bridge's receiver, but only once that receiver places rows: while it only acquires
//!    satellites, the phone's fixes keep placing them.
//! 4. **A receiver a search found**, standing down while a tier above sends sentences
//!    ([`StandDown::WhileAboveTalks`]), so a host reads one receiver at a time.
//! 5. **A typed-in position**, last, which marks the capture as test data.

use std::time::Duration;

use wartui_core::feed::FixFeed;
use wartui_core::gps::Gps;
use wartui_core::position::{PositionChain, StandDown, Tier};

/// A position the operator typed in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fixed {
    /// Degrees north.
    pub lat: f64,
    /// Degrees east.
    pub lon: f64,
    /// Meters above mean sea level, if given.
    pub alt: Option<f64>,
}

/// Every source a front end has for where a row was seen. Each is optional, and the handles are
/// clones of the ones the front end feeds.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// A receiver the operator named. First: naming it is an instruction.
    pub named: Option<Gps>,
    /// The bridge's receiver (`Gps::bridged`).
    pub bridge: Option<Gps>,
    /// The platform's location services. Below the bridge's receiver, and standing down while a
    /// tier above holds a fix run (`StandDown::WhileAboveFixed`), so a phone's location keeps
    /// placing rows until the bridge's receiver actually does.
    pub platform: Option<FixFeed>,
    /// A receiver a search found. Standing down while a tier above sends sentences
    /// (`StandDown::WhileAboveTalks`).
    pub searched: Option<Gps>,
    /// A typed-in position. Last, and it marks the capture as test data.
    pub fixed: Option<Fixed>,
}

impl Sources {
    /// The chain, in the order above, skipping what is absent, believing a fix up to `max_age`
    /// old.
    #[must_use]
    pub fn chain(&self, max_age: Duration) -> PositionChain {
        let named = self.named.iter().map(|gps| Tier::receiver(gps.clone()));
        let bridge = self.bridge.iter().map(|gps| Tier::receiver(gps.clone()));
        let platform = self
            .platform
            .iter()
            .map(|feed| Tier::fixes(feed.clone()).standing_down(StandDown::WhileAboveFixed));
        let searched = self
            .searched
            .iter()
            .map(|gps| Tier::receiver(gps.clone()).standing_down(StandDown::WhileAboveTalks));
        let fixed = self.fixed.iter().map(|at| Tier::fixed(at.lat, at.lon, at.alt));
        let tiers = named.chain(bridge).chain(platform).chain(searched).chain(fixed).collect();
        PositionChain::new(tiers, max_age)
    }

    /// Whether the scan waits for a fix: `wanted` (the operator's setting), no typed-in position,
    /// and a source that can give a fix. Under `simulated` the bridge's receiver is not one: a
    /// simulated bridge has no receiver.
    #[must_use]
    pub const fn waits_for_fix(&self, wanted: bool, simulated: bool) -> bool {
        let can_fix = self.named.is_some()
            || self.searched.is_some()
            || self.platform.is_some()
            || (self.bridge.is_some() && !simulated);
        wanted && self.fixed.is_none() && can_fix
    }

    /// Whether a capture placed by these sources holds test data: a typed-in position.
    #[must_use]
    pub const fn is_test_data(&self) -> bool {
        self.fixed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wartui_core::feed::{FixFeed, LocationFix, Pushed};
    use wartui_core::gps::Gps;
    use wartui_core::position::{DEFAULT_MAX_AGE, PositionSource};

    use super::{Fixed, Sources};

    /// A synthetic position, as `wartui-core`'s tests type in.
    const TYPED: Fixed = Fixed { lat: 37.7749, lon: -122.4194, alt: Some(16.0) };

    /// A fix whose latitude names which receiver gave it: 48 degrees and `lat_minutes`.
    fn gga_at(lat_minutes: &str) -> Vec<u8> {
        let body =
            format!("GPGGA,123519.00,48{lat_minutes},N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,");
        let sum = body.bytes().fold(0u8, |sum, byte| sum ^ byte);
        format!("${body}*{sum:02X}").into_bytes()
    }

    /// A sentence with no fix: a receiver acquiring satellites.
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76\r\n";

    /// A platform fix at a synthetic place, taken at `at_ms`.
    fn platform_fix(at_ms: i64) -> LocationFix {
        LocationFix {
            lat: 37.8,
            lon: -122.4,
            alt_msl: None,
            accuracy_m: Some(9.0),
            at_ms,
            mock: false,
        }
    }

    /// Every source, each its own handle, and the typed-in position.
    fn every_source() -> Sources {
        Sources {
            named: Some(Gps::for_reader(true, false, DEFAULT_MAX_AGE)),
            bridge: Some(Gps::bridged(DEFAULT_MAX_AGE)),
            platform: Some(FixFeed::new(DEFAULT_MAX_AGE)),
            searched: Some(Gps::for_reader(false, false, DEFAULT_MAX_AGE)),
            fixed: Some(TYPED),
        }
    }

    /// The latitude the chain resolves to at `now_ms`, and the kind of tier that gave it.
    fn answer(sources: &Sources, now_ms: i64) -> (Option<f64>, PositionSource) {
        let fix = sources.chain(DEFAULT_MAX_AGE).resolve(now_ms);
        (fix.lat.map(|lat| (lat * 1e4).round() / 1e4), fix.source)
    }

    #[test]
    fn position_sources_order_named_bridge_platform_searched_fixed_when_all_are_given() {
        let sources = every_source();
        let [named, bridge, searched] =
            [&sources.named, &sources.bridge, &sources.searched].map(|gps| gps.clone().unwrap());
        let platform = sources.platform.clone().unwrap();

        assert_eq!(answer(&sources, 10_000), (Some(37.7749), PositionSource::Static));
        searched.feed(&gga_at("06.000"), 10_000);
        assert_eq!(answer(&sources, 10_000), (Some(48.1), PositionSource::Gps));
        assert_eq!(platform.push(platform_fix(10_000), 10_000), Pushed::Taken);
        assert_eq!(answer(&sources, 10_000), (Some(37.8), PositionSource::Platform));
        bridge.feed(&gga_at("12.000"), 10_000);
        assert_eq!(answer(&sources, 10_000), (Some(48.2), PositionSource::Gps));
        named.feed(&gga_at("18.000"), 10_000);
        assert_eq!(answer(&sources, 10_000), (Some(48.3), PositionSource::Gps));
    }

    #[test]
    fn position_sources_platform_stands_down_when_bridge_receiver_holds_a_fix_run() {
        let bridge = Gps::bridged(DEFAULT_MAX_AGE);
        let platform = FixFeed::new(DEFAULT_MAX_AGE);
        let sources = Sources {
            bridge: Some(bridge.clone()),
            platform: Some(platform.clone()),
            ..Sources::default()
        };
        let chain = sources.chain(DEFAULT_MAX_AGE);
        chain.stand_down(1_000, true);
        assert!(*platform.wanted().borrow(), "the bridge's receiver holds no fix yet");
        bridge.feed(&gga_at("07.038"), 1_000);
        chain.stand_down(1_000, true);
        assert!(!*platform.wanted().borrow());
    }

    #[test]
    fn position_sources_platform_stays_wanted_when_bridge_receiver_only_talks() {
        // Acquiring satellites, the bridge's receiver places no rows; the phone's fixes do.
        let bridge = Gps::bridged(DEFAULT_MAX_AGE);
        let platform = FixFeed::new(DEFAULT_MAX_AGE);
        let sources = Sources {
            bridge: Some(bridge.clone()),
            platform: Some(platform.clone()),
            ..Sources::default()
        };
        let chain = sources.chain(DEFAULT_MAX_AGE);
        bridge.feed_bytes(GGA_NO_FIX, 1_000);
        assert!(bridge.is_talking(1_000));
        chain.stand_down(1_000, true);
        assert!(*platform.wanted().borrow());
    }

    #[test]
    fn position_sources_wait_for_fix_when_a_source_can_fix_and_operator_wants_it() {
        let bridge = || Some(Gps::bridged(DEFAULT_MAX_AGE));
        let host = |pinned| Some(Gps::for_reader(pinned, false, DEFAULT_MAX_AGE));
        let can_fix = [
            Sources { named: host(true), ..Sources::default() },
            Sources { bridge: bridge(), ..Sources::default() },
            Sources { platform: Some(FixFeed::new(DEFAULT_MAX_AGE)), ..Sources::default() },
            Sources { searched: host(false), ..Sources::default() },
        ];
        for sources in &can_fix {
            assert!(sources.waits_for_fix(true, false), "{sources:?}");
        }
        // Under the simulator, a host's or a phone's source still fixes.
        for sources in &can_fix[2..] {
            assert!(sources.waits_for_fix(true, true), "{sources:?}");
        }
    }

    #[test]
    fn position_sources_skip_the_wait_when_operator_does_not_want_it() {
        assert!(!every_source().waits_for_fix(false, false));
    }

    #[test]
    fn position_sources_skip_the_wait_when_a_position_is_typed_in() {
        assert!(!every_source().waits_for_fix(true, false));
    }

    #[test]
    fn position_sources_skip_the_wait_when_only_a_bridge_receiver_is_simulated() {
        // A simulated bridge has no receiver.
        let sources = Sources { bridge: Some(Gps::bridged(DEFAULT_MAX_AGE)), ..Sources::default() };
        assert!(!sources.waits_for_fix(true, true));
    }

    #[test]
    fn position_sources_skip_the_wait_when_there_is_no_source() {
        assert!(!Sources::default().waits_for_fix(true, false));
    }

    #[test]
    fn position_sources_are_test_data_when_a_position_is_typed_in() {
        assert!(Sources { fixed: Some(TYPED), ..Sources::default() }.is_test_data());
        let mut untyped = every_source();
        untyped.fixed = None;
        assert!(!untyped.is_test_data());
        // The typed-in position places rows, at the bottom of the chain.
        let fix = Sources { fixed: Some(TYPED), ..Sources::default() }
            .chain(Duration::from_secs(5))
            .resolve(0);
        assert_eq!((fix.lat, fix.alt), (Some(TYPED.lat), TYPED.alt));
    }
}
