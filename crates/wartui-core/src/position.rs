//! Where an observation was made.
//!
//! A chain of tiers resolved per record, in the order the caller builds it: the first tier that
//! holds a position places the row, and with none the row is unpositioned. **A record is never
//! dropped for want of a position**: an unpositioned observation still proves a network exists, and
//! export decides whether it can be uploaded.
//!
//! A tier is one of three providers ([`Provider`]):
//!
//! | Provider | Yields | Stored as |
//! |---|---|---|
//! | [`Provider::Receiver`] | an NMEA receiver's last fix, aged by arrival | `gps` |
//! | [`Provider::Fixes`] | a platform's typed fix ([`crate::feed`]), aged by its time | `platform` |
//! | [`Provider::Static`] | a configured position, always | `static` |
//!
//! A static tier always yields, so it shadows every tier below it. With nothing yielding, the row
//! stores `none`. Every receiver answers as [`PositionSource::Gps`]: a row records that a receiver
//! placed it, not which one.
//!
//! `wartui run` builds a host receiver pinned with `--gps`, then the bridge's receiver, then a host
//! receiver the search found, then `--lat`/`--lon`. A named port goes first because `--gps PATH`
//! is the operator's instruction. The bridge's receiver goes above a searched one because the
//! searched one stands down while the bridge's is sending sentences, and is then not read at all.
//!
//! **A tier may stand down for the tiers above it** ([`StandDown`]), so a device reads one source
//! at a time: [`PositionChain::stand_down`] suspends a receiver's search ([`Gps::set_suspended`])
//! or tells a feed's platform it may ease off ([`FixFeed::wanted`]). A rule keyed on sentences
//! ([`StandDown::WhileAboveTalks`]) lets the operator watch the tier above acquire satellites; one
//! keyed on a fix run ([`StandDown::WhileAboveFixed`]) keeps the lower tier until the upper one is
//! actually placing rows. The bridge's receiver counts only while a bridge is connected: its last
//! sentences outlive the link.
//!
//! Tiers are tried per record, so the source is stored per row: one capture can start indoors on a
//! typed-in position, gain satellites in the car park, and lose them in a tunnel.
//!
//! **A fix must be recent to be used**: past [`PositionChain::max_age`] the chain falls through to
//! the next tier. [`DEFAULT_MAX_AGE`] says what recent means.

use std::time::Duration;

use crate::feed::FixFeed;
use crate::gps::Gps;

/// How old a GPS fix may be and still be believed.
///
/// At 1 Hz this is five missed fixes, and more at a faster rate: enough to ride out a tunnel or a
/// dropped link, short enough that at 50 km/h the position is off by about seventy meters at most,
/// inside what a Wi-Fi observation means anyway.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(5);

/// Which kind of tier produced a fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    /// A live NMEA fix from a GPS receiver, the bridge's or the host's.
    Gps,
    /// A fix from a platform's location services, which may be network-derived.
    Platform,
    /// The operator's configured lat/lon. Constant for the run.
    Static,
    /// No position was available.
    None,
}

impl PositionSource {
    /// The token stored in `observation.pos_source`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gps => "gps",
            Self::Platform => "platform",
            Self::Static => "static",
            Self::None => "none",
        }
    }
}

/// A position, or the explicit absence of one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    /// Degrees north, if known.
    pub lat: Option<f64>,
    /// Degrees east, if known.
    pub lon: Option<f64>,
    /// Meters above mean sea level, if known.
    pub alt: Option<f64>,
    /// Horizontal accuracy in meters, if known.
    pub accuracy: Option<f64>,
    /// Which kind of tier this came from.
    pub source: PositionSource,
    /// When the fix itself was taken, in Unix milliseconds.
    ///
    /// Separate from the observation's receive time, so a stale fix on a fresh observation is
    /// visible.
    pub at_ms: Option<i64>,
}

impl Fix {
    /// The empty fix, which is what the bottom of the chain yields.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            lat: None,
            lon: None,
            alt: None,
            accuracy: None,
            source: PositionSource::None,
            at_ms: None,
        }
    }

    /// Whether this fix carries coordinates the WiGLE export can use.
    #[must_use]
    pub const fn is_located(&self) -> bool {
        self.lat.is_some() && self.lon.is_some()
    }
}

/// What a tier reads its position from.
#[derive(Debug, Clone)]
pub enum Provider {
    /// An NMEA receiver: a host puck or the bridge's.
    Receiver(Gps),
    /// Typed fixes from a platform's location services.
    Fixes(FixFeed),
    /// A configured position.
    Static(Fix),
}

/// When a tier stands down for the tiers above it ([`PositionChain::stand_down`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandDown {
    /// Never.
    Never,
    /// While a receiver tier above is sending sentences (`Gps::is_talking`), fix or not.
    WhileAboveTalks,
    /// While a tier above holds a fix run (`fix_run`).
    WhileAboveFixed,
}

/// One tier of the chain: where it reads a position, and when it stands down.
#[derive(Debug, Clone)]
pub struct Tier {
    /// Where the position comes from.
    pub provider: Provider,
    /// When this tier stands down for those above it.
    pub stand_down: StandDown,
}

impl Tier {
    /// A receiver's tier, which never stands down.
    #[must_use]
    pub const fn receiver(gps: Gps) -> Self {
        Self { provider: Provider::Receiver(gps), stand_down: StandDown::Never }
    }

    /// A platform feed's tier, which never stands down.
    #[must_use]
    pub const fn fixes(feed: FixFeed) -> Self {
        Self { provider: Provider::Fixes(feed), stand_down: StandDown::Never }
    }

    /// A position the operator typed in.
    ///
    /// `at_ms` is unset: a static position is as current as the operator's claim.
    #[must_use]
    pub const fn fixed(lat: f64, lon: f64, alt: Option<f64>) -> Self {
        let fix = Fix {
            lat: Some(lat),
            lon: Some(lon),
            alt,
            accuracy: None,
            source: PositionSource::Static,
            at_ms: None,
        };
        Self { provider: Provider::Static(fix), stand_down: StandDown::Never }
    }

    /// Stand this tier down by `rule`.
    #[must_use]
    pub const fn standing_down(mut self, rule: StandDown) -> Self {
        self.stand_down = rule;
        self
    }

    /// The receiver's last fix, or the feed's, if recent within `max_age` of `now_ms`, or the
    /// static position.
    fn yields(&self, now_ms: i64, max_age: Duration) -> Option<Fix> {
        match &self.provider {
            Provider::Receiver(gps) => {
                let (fix, received_at_ms) = gps.latest()?;
                is_fresh(received_at_ms, now_ms, max_age).then_some(fix)
            }
            Provider::Fixes(feed) => {
                let fix = feed.latest()?;
                is_fresh(fix.at_ms?, now_ms, max_age).then_some(fix)
            }
            Provider::Static(fix) => Some(*fix),
        }
    }

    /// The fix run this tier holds. A static tier holds none: it is test data.
    fn fix_run(&self, now_ms: i64) -> Option<i64> {
        match &self.provider {
            Provider::Receiver(gps) => gps.fix_run(now_ms),
            Provider::Fixes(feed) => feed.fix_run(now_ms),
            Provider::Static(_) => None,
        }
    }

    /// Whether this tier counts toward standing the tiers below it down. The bridge's receiver
    /// counts only while a bridge is connected.
    fn counts(&self, bridge_connected: bool) -> bool {
        match &self.provider {
            Provider::Receiver(gps) => bridge_connected || !gps.is_bridged(),
            Provider::Fixes(_) => true,
            Provider::Static(_) => false,
        }
    }

    /// Whether this tier is a receiver sending sentences.
    fn talks(&self, now_ms: i64, bridge_connected: bool) -> bool {
        self.counts(bridge_connected)
            && matches!(&self.provider, Provider::Receiver(gps) if gps.is_talking(now_ms))
    }

    /// Whether this tier holds a fix run.
    fn is_fixed(&self, now_ms: i64, bridge_connected: bool) -> bool {
        self.counts(bridge_connected) && self.fix_run(now_ms).is_some()
    }
}

/// The configured chain: tiers in the caller's order.
#[derive(Debug, Clone)]
pub struct PositionChain {
    tiers: Vec<Tier>,
    max_age: Duration,
}

impl Default for PositionChain {
    fn default() -> Self {
        Self::empty()
    }
}

impl PositionChain {
    /// A chain trying `tiers` in order, believing a fix up to `max_age` old.
    #[must_use]
    pub const fn new(tiers: Vec<Tier>, max_age: Duration) -> Self {
        Self { tiers, max_age }
    }

    /// A chain with no tiers at all: every observation is unpositioned.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(Vec::new(), DEFAULT_MAX_AGE)
    }

    /// How old a fix may be before the chain stops believing it.
    #[must_use]
    pub const fn max_age(&self) -> Duration {
        self.max_age
    }

    /// The bridge's receiver, if this chain has one: fed from the link by the runtime.
    #[must_use]
    pub fn bridge_receiver(&self) -> Option<&Gps> {
        self.receivers().find(|gps| gps.is_bridged())
    }

    /// The host's receiver, if this chain has one, for the UI to report on.
    #[must_use]
    pub fn host_receiver(&self) -> Option<&Gps> {
        self.receivers().find(|gps| !gps.is_bridged())
    }

    /// Resolve the best position available as of `now_ms`: the first tier that yields one.
    ///
    /// Takes the time rather than reading a clock, so the engine stays pure.
    #[must_use]
    pub fn resolve(&self, now_ms: i64) -> Fix {
        self.tiers
            .iter()
            .find_map(|tier| tier.yields(now_ms, self.max_age))
            .unwrap_or_else(Fix::none)
    }

    /// The fix run the chain holds as of `now_ms` ([`Gps::fix_run`], [`FixFeed::fix_run`]): the
    /// earliest among its tiers that hold one, so a second source flapping does not disturb a
    /// steady one. `None` while none holds a fix. A static position holds no fix: it is test data.
    #[must_use]
    pub fn fix_run(&self, now_ms: i64) -> Option<i64> {
        self.tiers.iter().filter_map(|tier| tier.fix_run(now_ms)).min()
    }

    /// Apply each tier's [`StandDown`] rule as of `now_ms`, judged by the tiers above it: suspend
    /// or resume a receiver's search, and want or unwant a feed's fixes. The bridge's receiver
    /// counts only while `bridge_connected`. Cheap and idempotent, so the runtime calls it on every
    /// tick; each provider logs only a change.
    pub fn stand_down(&self, now_ms: i64, bridge_connected: bool) {
        for (at, tier) in self.tiers.iter().enumerate() {
            let above = &self.tiers[..at];
            let down = match tier.stand_down {
                StandDown::Never => continue,
                StandDown::WhileAboveTalks => {
                    above.iter().any(|tier| tier.talks(now_ms, bridge_connected))
                }
                StandDown::WhileAboveFixed => {
                    above.iter().any(|tier| tier.is_fixed(now_ms, bridge_connected))
                }
            };
            match &tier.provider {
                Provider::Receiver(gps) => gps.set_suspended(down),
                Provider::Fixes(feed) => feed.set_wanted(!down),
                Provider::Static(_) => {}
            }
        }
    }

    /// Every receiver tier's receiver, in order.
    fn receivers(&self) -> impl Iterator<Item = &Gps> {
        self.tiers.iter().filter_map(|tier| match &tier.provider {
            Provider::Receiver(gps) => Some(gps),
            _ => None,
        })
    }
}

/// Whether something received at `at_ms` is at most `max_age` old as of `now_ms`.
///
/// A negative age is a host clock stepping back, not a fix from the future. Treating it as fresh
/// keeps positions across an NTP correction.
pub(crate) fn is_fresh(at_ms: i64, now_ms: i64, max_age: Duration) -> bool {
    // Saturating, because a `Duration` can hold more milliseconds than an `i64` and an operator is
    // allowed to type a silly number.
    let max_age_ms = i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX);
    now_ms.saturating_sub(at_ms) <= max_age_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{LocationFix, Pushed};

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
    const GSV: &[u8] = b"$GPGSV,1,1,00*79\r\n";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76\r\n";

    /// A chain of `tiers` at the default age limit.
    fn chain(tiers: Vec<Tier>) -> PositionChain {
        PositionChain::new(tiers, DEFAULT_MAX_AGE)
    }

    /// The tests' synthetic static position.
    fn fixed() -> Tier {
        Tier::fixed(37.7749, -122.4194, Some(16.0))
    }

    fn chain_with_gps() -> (Gps, PositionChain) {
        let gps = Gps::detached();
        let chain = chain(vec![Tier::receiver(gps.clone()), fixed()]);
        (gps, chain)
    }

    #[test]
    fn position_chain_resolves_none_when_empty() {
        // Never dropping a record is the point; the empty fix is how that is
        // expressed rather than an `Option` every caller has to unwrap.
        let fix = PositionChain::empty().resolve(0);
        assert_eq!(fix.source, PositionSource::None);
        assert!(!fix.is_located());
    }

    #[test]
    fn position_chain_resolves_unlocated_when_gps_fix_old() {
        let gps = Gps::detached();
        let chain = chain(vec![Tier::receiver(gps.clone())]);
        gps.feed(GGA, 0);
        let fix = chain.resolve(5_001);
        assert!(!fix.is_located());
    }

    #[test]
    fn position_chain_resolves_static_when_fixed() {
        let fix = chain(vec![fixed()]).resolve(0);
        assert_eq!(fix.source, PositionSource::Static);
        assert!(fix.is_located());
        assert_eq!(fix.alt, Some(16.0));
        // A typed-in position has no fix time and must not borrow the clock's.
        assert_eq!(fix.at_ms, None);
    }

    #[test]
    fn position_chain_falls_back_to_static_when_gps_has_no_fix_yet() {
        let (_gps, chain) = chain_with_gps();
        assert_eq!(chain.resolve(0).source, PositionSource::Static);
    }

    #[test]
    fn position_chain_prefers_gps_over_static_when_fix_fresh() {
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        let fix = chain.resolve(12_000);
        assert_eq!(fix.source, PositionSource::Gps);
        assert!((fix.lat.expect("a latitude") - 48.117_3).abs() < 1e-4);
        assert_eq!(fix.accuracy, Some(4.5), "the receiver's own error estimate rides along");
    }

    #[test]
    fn position_chain_falls_back_to_static_when_fix_past_max_age() {
        // The whole reason the chain takes a clock; `DEFAULT_MAX_AGE` has the
        // arithmetic.
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(15_000).source, PositionSource::Gps, "exactly at the limit");
        assert_eq!(chain.resolve(15_001).source, PositionSource::Static);
    }

    #[test]
    fn position_chain_falls_back_to_none_when_stale_and_no_static() {
        let gps = Gps::detached();
        let chain = chain(vec![Tier::receiver(gps.clone())]);
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(60_000).source, PositionSource::None);
    }

    #[test]
    fn position_chain_shadows_lower_tiers_when_static_is_above_them() {
        let gps = Gps::detached();
        let chain = chain(vec![fixed(), Tier::receiver(gps.clone())]);
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(10_000).source, PositionSource::Static);
    }

    /// A chain of the bridge's receiver, a searched host receiver and a static position, and the
    /// two receivers.
    fn chain_with_both() -> (Gps, Gps, PositionChain) {
        let (bridge, host) = (Gps::detached(), Gps::detached());
        let tiers = vec![Tier::receiver(bridge.clone()), Tier::receiver(host.clone()), fixed()];
        (bridge, host, chain(tiers))
    }

    /// A fix whose latitude names which receiver gave it.
    fn gga_at(lat_minutes: &str) -> Vec<u8> {
        let body =
            format!("GPGGA,123519.00,48{lat_minutes},N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,");
        let sum = body.bytes().fold(0u8, |sum, byte| sum ^ byte);
        format!("${body}*{sum:02X}").into_bytes()
    }

    #[test]
    fn position_chain_prefers_bridge_gps_over_searched_host_gps_when_both_fresh() {
        let (bridge, host, chain) = chain_with_both();
        host.feed(&gga_at("07.038"), 10_000);
        bridge.feed(&gga_at("30.000"), 10_000);
        let fix = chain.resolve(11_000);
        assert_eq!(fix.source, PositionSource::Gps);
        assert!((fix.lat.expect("a latitude") - 48.5).abs() < 1e-6, "{fix:?}");
    }

    #[test]
    fn position_chain_prefers_pinned_host_gps_over_bridge_gps_when_both_fresh() {
        // `--gps PATH` is the operator's instruction, so `wartui run` puts it above the bridge's.
        let (host, bridge) = (Gps::detached_pinned(), Gps::detached());
        let chain = chain(vec![Tier::receiver(host.clone()), Tier::receiver(bridge.clone())]);
        host.feed(&gga_at("07.038"), 10_000);
        bridge.feed(&gga_at("30.000"), 10_000);
        let fix = chain.resolve(11_000);
        assert!((fix.lat.expect("a latitude") - 48.117_3).abs() < 1e-4, "{fix:?}");
    }

    #[test]
    fn position_chain_falls_back_to_bridge_gps_when_pinned_host_fix_past_max_age() {
        let (host, bridge) = (Gps::detached_pinned(), Gps::detached());
        let chain = chain(vec![Tier::receiver(host.clone()), Tier::receiver(bridge.clone())]);
        host.feed(&gga_at("07.038"), 1_000);
        bridge.feed(&gga_at("30.000"), 10_000);
        let fix = chain.resolve(10_000);
        assert!((fix.lat.expect("a latitude") - 48.5).abs() < 1e-6, "{fix:?}");
    }

    #[test]
    fn position_chain_applies_max_age_when_not_the_default() {
        let gps = Gps::detached();
        let chain = PositionChain::new(vec![Tier::receiver(gps.clone())], Duration::from_secs(1));
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(11_000).source, PositionSource::Gps);
        assert_eq!(chain.resolve(11_001).source, PositionSource::None);
    }

    #[test]
    fn position_chain_falls_back_to_host_gps_when_bridge_fix_past_max_age() {
        let (bridge, host, chain) = chain_with_both();
        bridge.feed(&gga_at("30.000"), 1_000);
        host.feed(&gga_at("07.038"), 10_000);
        let fix = chain.resolve(10_000);
        assert!((fix.lat.expect("a latitude") - 48.117_3).abs() < 1e-4, "{fix:?}");
    }

    #[test]
    fn position_chain_falls_back_to_static_when_both_receivers_stale() {
        let (bridge, host, chain) = chain_with_both();
        bridge.feed(&gga_at("30.000"), 1_000);
        host.feed(&gga_at("07.038"), 1_000);
        assert_eq!(chain.resolve(6_001).source, PositionSource::Static);
    }

    #[test]
    fn position_chain_uses_bridge_gps_when_chain_has_no_host_gps() {
        let bridge = Gps::bridged(DEFAULT_MAX_AGE);
        let chain = chain(vec![Tier::receiver(bridge.clone())]);
        assert_eq!(chain.resolve(0).source, PositionSource::None);
        bridge.feed(GGA, 1_000);
        assert_eq!(chain.resolve(6_000).source, PositionSource::Gps);
        assert_eq!(chain.resolve(6_001).source, PositionSource::None);
    }

    #[test]
    fn position_chain_names_receivers_by_whether_they_are_bridged() {
        let (bridge, host) = (Gps::bridged(DEFAULT_MAX_AGE), Gps::detached());
        let chain = chain(vec![Tier::receiver(bridge), Tier::receiver(host), fixed()]);
        assert!(chain.bridge_receiver().is_some_and(Gps::is_bridged));
        assert!(chain.host_receiver().is_some_and(|gps| !gps.is_bridged()));
        assert!(PositionChain::empty().bridge_receiver().is_none());
        assert!(PositionChain::empty().host_receiver().is_none());
    }

    #[test]
    fn position_chain_reports_earliest_fix_run_when_both_receivers_fix() {
        let (bridge, host, chain) = chain_with_both();
        assert_eq!(chain.fix_run(1_000), None);
        host.feed(GGA, 1_000);
        bridge.feed(GGA, 2_000);
        assert_eq!(chain.fix_run(2_000), Some(1_000));
        // The host's goes quiet past the gap; the bridge's still holds.
        bridge.feed(GGA, 3_000);
        assert_eq!(chain.fix_run(3_000), Some(2_000));
    }

    #[test]
    fn position_chain_holds_no_fix_run_when_only_static() {
        assert_eq!(chain(vec![Tier::fixed(37.7749, -122.4194, None)]).fix_run(0), None);
    }

    #[test]
    fn position_chain_keeps_gps_when_clock_steps_back() {
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(9_000).source, PositionSource::Gps);
    }

    /// A platform fix at a synthetic place, taken at `at_ms`.
    fn platform_fix(at_ms: i64) -> LocationFix {
        LocationFix {
            lat: 37.8,
            lon: -122.4,
            alt_msl: Some(12.0),
            accuracy_m: Some(9.0),
            at_ms,
            mock: false,
        }
    }

    /// A feed holding a fix taken at `at_ms`, pushed then.
    fn feed_holding(at_ms: i64) -> FixFeed {
        let feed = FixFeed::new(DEFAULT_MAX_AGE);
        assert_eq!(feed.push(platform_fix(at_ms), at_ms), Pushed::Taken);
        feed
    }

    #[test]
    fn position_chain_resolves_platform_when_feed_fix_is_fresh() {
        let chain = chain(vec![Tier::fixes(feed_holding(10_000)), fixed()]);
        let fix = chain.resolve(11_000);
        assert_eq!(fix.source, PositionSource::Platform);
        assert_eq!(fix.lat, Some(37.8));
        assert_eq!(fix.alt, Some(12.0));
        assert_eq!(fix.accuracy, Some(9.0));
        assert_eq!(fix.at_ms, Some(10_000));
    }

    #[test]
    fn position_chain_falls_past_feed_when_fix_time_is_past_max_age() {
        // Pushed just now, but taken 4 s ago: the fix ages by its own time, so it is past the
        // limit 2 s later, where a receiver's arrival stamp would still hold for 5.
        let feed = FixFeed::new(DEFAULT_MAX_AGE);
        assert_eq!(feed.push(platform_fix(6_000), 10_000), Pushed::Taken);
        let chain = chain(vec![Tier::fixes(feed), fixed()]);
        assert_eq!(chain.resolve(10_000).source, PositionSource::Platform);
        assert_eq!(chain.resolve(12_000).source, PositionSource::Static);
    }

    #[test]
    fn position_chain_falls_past_feed_when_fix_stamped_ahead_is_max_age_past_arrival() {
        // Stamped two minutes ahead of the clock: it ages from its arrival, not its stamp.
        let feed = FixFeed::new(DEFAULT_MAX_AGE);
        assert_eq!(feed.push(platform_fix(130_000), 10_000), Pushed::Taken);
        let chain = chain(vec![Tier::fixes(feed), fixed()]);
        assert_eq!(chain.resolve(15_000).source, PositionSource::Platform, "exactly at the limit");
        assert_eq!(chain.resolve(15_001).source, PositionSource::Static);
    }

    #[test]
    fn position_chain_resolves_in_callers_order_when_feed_is_above_receiver() {
        let gps = Gps::detached();
        gps.feed(GGA, 10_000);
        let chain = chain(vec![Tier::fixes(feed_holding(10_000)), Tier::receiver(gps)]);
        assert_eq!(chain.resolve(10_000).source, PositionSource::Platform);
    }

    #[test]
    fn position_chain_resolves_in_callers_order_when_receiver_is_above_feed() {
        let gps = Gps::detached();
        gps.feed(GGA, 10_000);
        let chain = chain(vec![Tier::receiver(gps), Tier::fixes(feed_holding(10_000))]);
        assert_eq!(chain.resolve(10_000).source, PositionSource::Gps);
    }

    #[test]
    fn position_chain_reports_feed_fix_run_when_feed_holds_one() {
        let chain = chain(vec![Tier::fixes(feed_holding(1_000)), fixed()]);
        assert_eq!(chain.fix_run(2_000), Some(1_000));
        assert_eq!(chain.fix_run(4_001), None, "past the feed's gap");
    }

    #[test]
    fn position_chain_unwants_feed_when_receiver_above_holds_fix() {
        let (gps, feed) = (Gps::detached(), FixFeed::new(DEFAULT_MAX_AGE));
        let lower = Tier::fixes(feed.clone()).standing_down(StandDown::WhileAboveFixed);
        let chain = chain(vec![Tier::receiver(gps.clone()), lower]);
        chain.stand_down(1_000, true);
        assert!(*feed.wanted().borrow(), "the receiver holds no fix yet");
        gps.feed(GGA, 1_000);
        chain.stand_down(1_000, true);
        assert!(!*feed.wanted().borrow());
    }

    #[test]
    fn position_chain_keeps_feed_wanted_when_receiver_above_talks_without_fix() {
        let (gps, feed) = (Gps::bridged(DEFAULT_MAX_AGE), FixFeed::new(DEFAULT_MAX_AGE));
        let lower = Tier::fixes(feed.clone()).standing_down(StandDown::WhileAboveFixed);
        let chain = chain(vec![Tier::receiver(gps.clone()), lower]);
        gps.feed_bytes(GGA_NO_FIX, 1_000);
        assert!(gps.is_talking(1_000));
        chain.stand_down(1_000, true);
        assert!(*feed.wanted().borrow());
    }

    #[test]
    fn position_chain_wants_feed_again_when_receiver_above_loses_its_fix_run() {
        let (gps, feed) = (Gps::detached(), FixFeed::new(DEFAULT_MAX_AGE));
        let lower = Tier::fixes(feed.clone()).standing_down(StandDown::WhileAboveFixed);
        let chain = chain(vec![Tier::receiver(gps.clone()), lower]);
        gps.feed(GGA, 1_000);
        chain.stand_down(1_000, true);
        assert!(!*feed.wanted().borrow());
        // Past `gps::FIX_GAP` with no report.
        chain.stand_down(2_501, true);
        assert!(*feed.wanted().borrow());
    }

    #[test]
    fn position_chain_keeps_feed_wanted_when_feed_is_above_the_receiver() {
        let (gps, feed) = (Gps::detached(), FixFeed::new(DEFAULT_MAX_AGE));
        let upper = Tier::fixes(feed.clone()).standing_down(StandDown::WhileAboveFixed);
        let chain = chain(vec![upper, Tier::receiver(gps.clone())]);
        gps.feed(GGA, 1_000);
        chain.stand_down(1_000, true);
        assert!(*feed.wanted().borrow(), "nothing is above the feed");
    }

    #[test]
    fn position_chain_keeps_feed_wanted_when_bridge_receiver_is_fixed_but_no_bridge_is_connected() {
        let (bridge, feed) = (Gps::bridged(DEFAULT_MAX_AGE), FixFeed::new(DEFAULT_MAX_AGE));
        let lower = Tier::fixes(feed.clone()).standing_down(StandDown::WhileAboveFixed);
        let chain = chain(vec![Tier::receiver(bridge.clone()), lower]);
        bridge.feed(GGA, 1_000);
        chain.stand_down(1_000, false);
        assert!(*feed.wanted().borrow());
        chain.stand_down(1_000, true);
        assert!(!*feed.wanted().borrow(), "the same fix counts once a bridge is connected");
    }

    #[test]
    fn position_chain_leaves_tier_alone_when_its_rule_is_never() {
        let (bridge, host) = (Gps::bridged(DEFAULT_MAX_AGE), Gps::detached());
        let feed = FixFeed::new(DEFAULT_MAX_AGE);
        let tiers = vec![
            Tier::receiver(bridge.clone()),
            Tier::receiver(host.clone()),
            Tier::fixes(feed.clone()),
        ];
        let chain = chain(tiers);
        bridge.feed_bytes(GSV, 1_000);
        bridge.feed(GGA, 1_000);
        chain.stand_down(1_000, true);
        assert!(!host.suspended());
        assert!(*feed.wanted().borrow());
    }

    /// The chain `wartui run` builds for a searched host receiver: the bridge's, then the host's
    /// standing down while the bridge's talks.
    fn searched_under_bridge() -> (Gps, Gps, PositionChain) {
        let (host, bridge) = (Gps::detached(), Gps::bridged(DEFAULT_MAX_AGE));
        let tiers = vec![
            Tier::receiver(bridge.clone()),
            Tier::receiver(host.clone()).standing_down(StandDown::WhileAboveTalks),
        ];
        (host, bridge, chain(tiers))
    }

    #[test]
    fn position_chain_suspends_host_gps_when_bridge_sends_no_fix_sentences() {
        // No sky: sentences every epoch, and the operator watches the bridge's receiver acquire.
        let (host, bridge, chain) = searched_under_bridge();
        chain.stand_down(1_000, true);
        assert!(!host.suspended(), "a bridge whose receiver says nothing");
        bridge.feed_bytes(GGA_NO_FIX, 1_000);
        bridge.feed_bytes(GSV, 1_000);
        chain.stand_down(1_000, false);
        assert!(!host.suspended(), "sentences with no bridge connected");
        chain.stand_down(1_000, true);
        assert!(host.suspended());
    }

    #[test]
    fn position_chain_resumes_host_gps_when_bridge_sentences_go_stale() {
        let (host, bridge, chain) = searched_under_bridge();
        bridge.feed_bytes(GSV, 1_000);
        chain.stand_down(6_000, true);
        assert!(host.suspended());
        chain.stand_down(6_001, true);
        assert!(!host.suspended());
    }

    #[test]
    fn position_chain_keeps_host_gps_searching_when_bridge_reconnects_until_a_new_sentence() {
        let (host, bridge, chain) = searched_under_bridge();
        bridge.feed_bytes(GSV, 1_000);
        // What `runtime::drive` does on `Connected`: the old sentence is within the age limit, but
        // from before.
        bridge.reset_stream();
        chain.stand_down(2_000, true);
        assert!(!host.suspended());
        bridge.feed_bytes(GSV, 3_000);
        chain.stand_down(3_000, true);
        assert!(host.suspended());
    }
}
