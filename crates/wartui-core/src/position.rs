//! Where an observation was made.
//!
//! A fallback chain, resolved fresh for every record: host GPS, then a static
//! configured position, then nothing. **A record is never dropped for want of a
//! position** — an observation with no coordinates is still evidence a network
//! exists, and the export is where the question of whether it can be uploaded
//! gets asked.
//!
//! Only the static and empty tiers exist in this build; the GPS tier arrives in
//! Phase 6 and slots in at the top of [`PositionChain::resolve`] without the
//! store or the export changing at all. That is the whole reason the source is
//! recorded per row rather than per session.

/// Which tier of the chain produced a fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    /// A live NMEA fix from a GPS on the host. Not implemented yet.
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
    /// Deliberately separate from the observation's receive time: a stale GPS
    /// fix attached to a fresh observation is a real and invisible error
    /// otherwise, and this is what makes the staleness visible.
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
#[derive(Debug, Clone, Default)]
pub struct PositionChain {
    fixed: Option<Fix>,
}

impl PositionChain {
    /// A chain with no sources at all: every observation is unpositioned.
    #[must_use]
    pub const fn empty() -> Self {
        Self { fixed: None }
    }

    /// A chain whose only tier is a position the operator typed in.
    ///
    /// `at_ms` is left unset because a static position has no fix time — it is
    /// as current as the operator's claim about it and no more.
    #[must_use]
    pub const fn fixed(lat: f64, lon: f64, alt: Option<f64>) -> Self {
        Self {
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

    /// Resolve the best position available right now.
    #[must_use]
    pub fn resolve(&self) -> Fix {
        // Phase 6 puts a GPS tier here, ahead of the static one.
        self.fixed.unwrap_or_else(Fix::none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_chain_still_yields_a_fix() {
        // Never dropping a record is the point; the empty fix is how that is
        // expressed rather than an `Option` every caller has to unwrap.
        let fix = PositionChain::empty().resolve();
        assert_eq!(fix.source, PositionSource::None);
        assert!(!fix.is_located());
    }

    #[test]
    fn a_static_position_is_reported_as_static() {
        let fix = PositionChain::fixed(37.7749, -122.4194, Some(16.0)).resolve();
        assert_eq!(fix.source, PositionSource::Static);
        assert!(fix.is_located());
        assert_eq!(fix.alt, Some(16.0));
        // A typed-in position has no fix time and must not borrow the clock's.
        assert_eq!(fix.at_ms, None);
    }
}
