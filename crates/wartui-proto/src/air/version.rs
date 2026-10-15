use core::fmt;

/// A node firmware version, as `firmware/node/Cargo.toml` states it and every heartbeat
/// carries it.
///
/// Before 1.0 a minor bump may change the wire and a patch bump may not, so two versions
/// agreeing on major and minor speak the same layouts ([`Version::speaks_with`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// Major version.
    pub major: u8,
    /// Minor version. Before 1.0, a change here is a wire change.
    pub minor: u8,
    /// Patch version. Never a wire change.
    pub patch: u8,
}

impl Version {
    /// A version from its three parts.
    #[must_use]
    pub const fn new(major: u8, minor: u8, patch: u8) -> Self {
        Self { major, minor, patch }
    }

    /// Whether a node on `other` speaks this build's layouts: the same major and minor,
    /// whatever the patch.
    #[must_use]
    pub const fn speaks_with(&self, other: &Self) -> bool {
        self.major == other.major && self.minor == other.minor
    }
}

/// `0.1.0`.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The node firmware this tree builds, and the version its heartbeats announce.
///
/// Mirrors `version` in `firmware/node/Cargo.toml`; `crates/wartui-proto/tests/version.rs`
/// fails when the two differ, so every firmware bump edits both.
pub const NODE_VERSION: Version = Version::new(0, 1, 0);
