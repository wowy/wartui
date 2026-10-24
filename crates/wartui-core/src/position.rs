//! Where an observation was made.
//!
//! A fallback chain resolved per record: a host GPS whose port the operator named, then the
//! bridge's GPS receiver, then a host GPS the search found, then a static configured position, then
//! nothing. **A record is never dropped for want of a position**: an unpositioned observation still
//! proves a network exists, and export decides whether it can be uploaded.
//!
//! A named port goes first because `--gps PATH` is the operator's instruction. The bridge's
//! receiver goes above a searched one because the host's search stands down while that receiver
//! is sending sentences ([`Gps::set_suspended`]), and it is then the only receiver being read.
//! Every receiver answers as [`PositionSource::Gps`]: a row records that a receiver placed it, not
//! which one.
//!
//! Tiers are tried per record, so the source is stored per row: one capture can start indoors on a
//! typed-in position, gain satellites in the car park, and lose them in a tunnel.
//!
//! **A fix must be recent to be used**: past [`PositionChain::max_age`] the chain falls through to
//! the next tier. [`DEFAULT_MAX_AGE`] says what recent means.

use std::time::Duration;

use crate::gps::Gps;

/// How old a GPS fix may be and still be believed.
///
/// At 1 Hz this is five missed fixes, and more at a faster rate: enough to ride out a tunnel or a
/// dropped link, short enough that at 50 km/h the position is off by about seventy meters at most,
/// inside what a Wi-Fi observation means anyway.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(5);

/// Which tier of the chain produced a fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    /// A live NMEA fix from a GPS receiver, the bridge's or the host's.
    Gps,
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
    /// Meters above the ellipsoid, if known.
    pub alt: Option<f64>,
    /// Horizontal accuracy in meters, if known.
    pub accuracy: Option<f64>,
    /// Which tier of the chain this came from.
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

/// The configured fallback chain.
#[derive(Debug, Clone)]
pub struct PositionChain {
    bridge_gps: Option<Gps>,
    gps: Option<Gps>,
    max_age: Duration,
    fixed: Option<Fix>,
}

impl Default for PositionChain {
    fn default() -> Self {
        Self::empty()
    }
}

impl PositionChain {
    /// A chain with no sources at all: every observation is unpositioned.
    #[must_use]
    pub const fn empty() -> Self {
        Self { bridge_gps: None, gps: None, max_age: DEFAULT_MAX_AGE, fixed: None }
    }

    /// A chain whose only tier is a position the operator typed in.
    ///
    /// `at_ms` is unset: a static position is as current as the operator's claim.
    #[must_use]
    pub const fn fixed(lat: f64, lon: f64, alt: Option<f64>) -> Self {
        Self {
            bridge_gps: None,
            gps: None,
            max_age: DEFAULT_MAX_AGE,
            fixed: Some(Fix {
                lat: Some(lat),
                lon: Some(lon),
                alt,
                accuracy: None,
                source: PositionSource::Static,
                at_ms: None,
            }),
        }
    }

    /// Add the host's receiver, above the static position: that is what the rows carry until the
    /// first fix lands, and whenever the receiver goes quiet for longer than [`Self::max_age`].
    /// A pinned one ([`Gps::is_pinned`]) also goes above the bridge's receiver.
    #[must_use]
    pub fn with_gps(mut self, gps: Gps) -> Self {
        self.gps = Some(gps);
        self
    }

    /// Add the bridge's receiver, above a host receiver the search found.
    #[must_use]
    pub fn with_bridge_gps(mut self, gps: Gps) -> Self {
        self.bridge_gps = Some(gps);
        self
    }

    /// How old a fix may be before every receiver in the chain falls through. Defaults to
    /// [`DEFAULT_MAX_AGE`].
    #[must_use]
    pub const fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The host's receiver, if this chain has one, for the UI to report on.
    #[must_use]
    pub const fn gps(&self) -> Option<&Gps> {
        self.gps.as_ref()
    }

    /// The bridge's receiver, if this chain has one: fed from the link by the runtime.
    #[must_use]
    pub const fn bridge_gps(&self) -> Option<&Gps> {
        self.bridge_gps.as_ref()
    }

    /// How old a fix may be before the chain stops believing it.
    #[must_use]
    pub const fn max_age(&self) -> Duration {
        self.max_age
    }

    /// Resolve the best position available as of `now_ms`.
    ///
    /// Takes the time rather than reading a clock, so the engine stays pure.
    #[must_use]
    pub fn resolve(&self, now_ms: i64) -> Fix {
        let pinned = self.gps.as_ref().filter(|gps| gps.is_pinned());
        let searched = self.gps.as_ref().filter(|gps| !gps.is_pinned());
        [pinned, self.bridge_gps.as_ref(), searched]
            .into_iter()
            .find_map(|gps| self.fresh(gps?, now_ms))
            .or(self.fixed)
            .unwrap_or_else(Fix::none)
    }

    /// The receiver's last fix, if it arrived within [`Self::max_age`] of `now_ms`.
    fn fresh(&self, gps: &Gps, now_ms: i64) -> Option<Fix> {
        let (fix, received_at_ms) = gps.latest()?;
        is_fresh(received_at_ms, now_ms, self.max_age).then_some(fix)
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

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";

    fn chain_with_gps() -> (Gps, PositionChain) {
        let gps = Gps::detached();
        let chain = PositionChain::fixed(37.7749, -122.4194, Some(16.0)).with_gps(gps.clone());
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
        let chain = PositionChain::empty().with_gps(gps.clone());
        gps.feed(GGA, 0);
        let fix = chain.resolve(5_001);
        assert!(!fix.is_located());
    }

    #[test]
    fn position_chain_resolves_static_when_fixed() {
        let fix = PositionChain::fixed(37.7749, -122.4194, Some(16.0)).resolve(0);
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
        let chain = PositionChain::empty().with_gps(gps.clone());
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(60_000).source, PositionSource::None);
    }

    /// A chain with both receivers and a static position, and the two receivers.
    fn chain_with_both() -> (Gps, Gps, PositionChain) {
        let (host, chain) = chain_with_gps();
        let bridge = Gps::detached();
        (bridge.clone(), host, chain.with_bridge_gps(bridge))
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
        // `--gps PATH` is the operator's instruction, so it outranks the bridge's receiver.
        let (host, bridge) = (Gps::detached_pinned(), Gps::detached());
        let chain = PositionChain::empty().with_gps(host.clone()).with_bridge_gps(bridge.clone());
        host.feed(&gga_at("07.038"), 10_000);
        bridge.feed(&gga_at("30.000"), 10_000);
        let fix = chain.resolve(11_000);
        assert!((fix.lat.expect("a latitude") - 48.117_3).abs() < 1e-4, "{fix:?}");
    }

    #[test]
    fn position_chain_falls_back_to_bridge_gps_when_pinned_host_fix_past_max_age() {
        let (host, bridge) = (Gps::detached_pinned(), Gps::detached());
        let chain = PositionChain::empty().with_gps(host.clone()).with_bridge_gps(bridge.clone());
        host.feed(&gga_at("07.038"), 1_000);
        bridge.feed(&gga_at("30.000"), 10_000);
        let fix = chain.resolve(10_000);
        assert!((fix.lat.expect("a latitude") - 48.5).abs() < 1e-6, "{fix:?}");
    }

    #[test]
    fn position_chain_applies_max_age_when_set_before_receivers() {
        // One setting, whatever order the chain was built in.
        let gps = Gps::detached();
        let chain =
            PositionChain::empty().with_max_age(Duration::from_secs(1)).with_gps(gps.clone());
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
        let bridge = Gps::detached();
        let chain = PositionChain::empty().with_bridge_gps(bridge.clone());
        assert_eq!(chain.resolve(0).source, PositionSource::None);
        bridge.feed(GGA, 1_000);
        assert_eq!(chain.resolve(6_000).source, PositionSource::Gps);
        assert_eq!(chain.resolve(6_001).source, PositionSource::None);
    }

    #[test]
    fn position_chain_keeps_gps_when_clock_steps_back() {
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(9_000).source, PositionSource::Gps);
    }
}
