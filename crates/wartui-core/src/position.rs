//! Where an observation was made.
//!
//! A fallback chain, resolved fresh for every record: a live GPS on the host,
//! then a static configured position, then nothing. **A record is never dropped
//! for want of a position** — an observation with no coordinates is still
//! evidence a network exists, and the export is where the question of whether
//! it can be uploaded gets asked.
//!
//! The tiers are tried in that order per record, which is why the source is stored
//! per row rather than per session: one capture can begin indoors on a typed-in
//! position, pick up satellites in the car park, and lose them in a tunnel.
//!
//! **A fix has to be recent to be used at all**, which is the interesting half —
//! past [`PositionChain::max_age`] the chain falls through to the tier below. See
//! [`DEFAULT_MAX_AGE`] for what recent means and why.

use std::time::Duration;

use crate::gps::Gps;

/// How old a GPS fix may be and still be believed.
///
/// Receivers emit at 1 Hz, so this is five missed sentences — long enough to
/// ride out a tunnel or a burst of dropped serial, short enough that at 50 km/h
/// the position is wrong by at most about seventy metres, which is inside what
/// a Wi-Fi observation means anyway.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(5);

/// Which tier of the chain produced a fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    /// A live NMEA fix from a GPS on the host.
    Gps,
    /// The operator's configured lat/lon. Constant for the session.
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
    /// Metres above the ellipsoid, if known.
    pub alt: Option<f64>,
    /// Horizontal accuracy in metres, if known.
    pub accuracy: Option<f64>,
    /// Which tier of the chain this came from.
    pub source: PositionSource,
    /// When the fix itself was taken, in Unix milliseconds.
    ///
    /// Separate from the observation's receive time, which is what makes a stale fix
    /// attached to a fresh observation visible rather than invisible.
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
        Self { gps: None, max_age: DEFAULT_MAX_AGE, fixed: None }
    }

    /// A chain whose only tier is a position the operator typed in.
    ///
    /// `at_ms` is unset: a static position is as current as the operator's claim.
    #[must_use]
    pub const fn fixed(lat: f64, lon: f64, alt: Option<f64>) -> Self {
        Self {
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

    /// Put a receiver at the top of the chain, above whatever is already in it.
    ///
    /// Both tiers together: the static position is what the rows carry until the
    /// first fix lands, and whenever the receiver goes quiet for longer than `max_age`.
    #[must_use]
    pub fn with_gps(mut self, gps: Gps, max_age: Duration) -> Self {
        self.gps = Some(gps);
        self.max_age = max_age;
        self
    }

    /// The receiver, if this chain has one, for the UI to report on.
    #[must_use]
    pub const fn gps(&self) -> Option<&Gps> {
        self.gps.as_ref()
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
        if let Some((fix, received_at_ms)) = self.gps.as_ref().and_then(Gps::latest) {
            let age_ms = now_ms.saturating_sub(received_at_ms);
            // A negative age is a host clock that stepped backwards, not a fix
            // from the future; treating it as fresh keeps the position of
            // everything captured across an NTP correction.
            if age_ms <= max_age_ms(self.max_age) {
                return fix;
            }
        }
        self.fixed.unwrap_or_else(Fix::none)
    }
}

/// Saturating, because a [`Duration`] can hold more milliseconds than an `i64`
/// and an operator is allowed to type a silly number.
fn max_age_ms(max_age: Duration) -> i64 {
    i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";

    fn chain_with_gps() -> (Gps, PositionChain) {
        let gps = Gps::detached();
        let chain = PositionChain::fixed(37.7749, -122.4194, Some(16.0))
            .with_gps(gps.clone(), DEFAULT_MAX_AGE);
        (gps, chain)
    }

    #[test]
    fn an_empty_chain_still_yields_a_fix() {
        // Never dropping a record is the point; the empty fix is how that is
        // expressed rather than an `Option` every caller has to unwrap.
        let fix = PositionChain::empty().resolve(0);
        assert_eq!(fix.source, PositionSource::None);
        assert!(!fix.is_located());
    }

    #[test]
    fn a_static_position_is_reported_as_static() {
        let fix = PositionChain::fixed(37.7749, -122.4194, Some(16.0)).resolve(0);
        assert_eq!(fix.source, PositionSource::Static);
        assert!(fix.is_located());
        assert_eq!(fix.alt, Some(16.0));
        // A typed-in position has no fix time and must not borrow the clock's.
        assert_eq!(fix.at_ms, None);
    }

    #[test]
    fn a_receiver_that_has_not_answered_yet_leaves_the_static_position_in_place() {
        let (_gps, chain) = chain_with_gps();
        assert_eq!(chain.resolve(0).source, PositionSource::Static);
    }

    #[test]
    fn a_fresh_fix_outranks_what_the_operator_typed_in() {
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        let fix = chain.resolve(12_000);
        assert_eq!(fix.source, PositionSource::Gps);
        assert!((fix.lat.expect("a latitude") - 48.117_3).abs() < 1e-4);
        assert_eq!(fix.accuracy, Some(4.5), "the receiver's own error estimate rides along");
    }

    #[test]
    fn a_fix_older_than_the_limit_stops_being_believed() {
        // The whole reason the chain takes a clock; `DEFAULT_MAX_AGE` has the
        // arithmetic.
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(15_000).source, PositionSource::Gps, "exactly at the limit");
        assert_eq!(chain.resolve(15_001).source, PositionSource::Static);
    }

    #[test]
    fn a_stale_fix_with_nothing_beneath_it_falls_all_the_way_through() {
        let gps = Gps::detached();
        let chain = PositionChain::empty().with_gps(gps.clone(), DEFAULT_MAX_AGE);
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(60_000).source, PositionSource::None);
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_discard_the_fix() {
        let (gps, chain) = chain_with_gps();
        gps.feed(GGA, 10_000);
        assert_eq!(chain.resolve(9_000).source, PositionSource::Gps);
    }
}
