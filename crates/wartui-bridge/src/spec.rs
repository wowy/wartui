//! How a person names a bridge, for either link. It holds no I/O.

use wartui_proto::mac::{self, Mac};

/// How a bridge was named on the command line.
///
/// A MAC is worth accepting because it is the only name that survives everything:
/// re-enumeration moves a device node, replugging into another socket moves a
/// `by-path` name, and an ESP32's address moves only when the board does. Its last
/// few octets are worth accepting too, because they are how the fleet table and
/// `wartui ports` are read aloud: an operator says `00:08`, not the whole address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeSpec {
    /// A device path, opened as given and never traded for another.
    Path(String),
    /// A P4 bridge's address, matched against the `Ready.mac` a [`probe`] brings back.
    ///
    /// [`probe`]: crate::serial::probe
    Mac(Mac),
    /// The last one to five octets of a P4 bridge's address, matched the same way. It
    /// names a board only when exactly one attached bridge answers ending with them.
    Tail(Vec<u8>),
}

impl BridgeSpec {
    /// Whether a board at this address is one this spec could name.
    ///
    /// A path names no address, so it matches none: whether a path reaches a board is
    /// a question for the OS, not for a list. A tail matching here is not yet a board
    /// named — several may match, and [`named_board`] is where that is refused.
    ///
    /// [`named_board`]: crate::serial::named_board
    #[must_use]
    pub fn matches_address(&self, mac: &Mac) -> bool {
        match self {
            Self::Path(_) => false,
            Self::Mac(wanted) => wanted == mac,
            Self::Tail(tail) => mac.ends_with(tail),
        }
    }
}

impl std::str::FromStr for BridgeSpec {
    type Err = std::convert::Infallible;

    /// Six colon-separated hex pairs is an address, and one to five is the end of
    /// one; everything else is a path. Nothing else can be: no device node is spelled
    /// that way. Each pair is exactly two hex digits, so `0:08` stays a path rather
    /// than being read as a guess at what was meant.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if let Some(mac) = mac::parse(text) {
            return Ok(Self::Mac(mac));
        }
        Ok(parse_tail(text).map_or_else(|| Self::Path(text.to_owned()), Self::Tail))
    }
}

/// One to five colon-separated pairs of hex digits, as the octets they spell.
fn parse_tail(text: &str) -> Option<Vec<u8>> {
    let octets = text.split(':').map(mac::pair).collect::<Option<Vec<u8>>>()?;
    (1..=5).contains(&octets.len()).then_some(octets)
}

impl std::fmt::Display for BridgeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(path) => f.write_str(path),
            Self::Mac(mac) => write!(f, "{}", mac::full(mac)),
            Self::Tail(tail) => write!(f, "{}", mac::octets(tail)),
        }
    }
}
