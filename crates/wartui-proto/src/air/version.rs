use core::fmt;

/// A node firmware version, as `firmware/node/Cargo.toml` states it and every heartbeat
/// carries it.
///
/// Before 1.0 a minor bump may change the wire and a patch bump may not, so two versions
/// agreeing on major and minor speak the same layouts ([`Version::compat`]).
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

    /// How a node on `node` stands against a host on `self`.
    #[must_use]
    pub const fn compat(&self, node: &Self) -> Compat {
        let host = (self.major, self.minor);
        let theirs = (node.major, node.minor);
        if theirs.0 < host.0 || (theirs.0 == host.0 && theirs.1 < host.1) {
            Compat::NodeBehind
        } else if theirs.0 != host.0 || theirs.1 != host.1 {
            Compat::NodeAhead
        } else if node.patch < self.patch {
            Compat::PatchBehind
        } else if node.patch > self.patch {
            Compat::PatchAhead
        } else {
            Compat::Same
        }
    }

    /// How a node on this version stands against the host this tree builds
    /// ([`NODE_VERSION`]). Every check of whether a node can be driven goes through here.
    #[must_use]
    pub const fn compat_with_host(&self) -> Compat {
        NODE_VERSION.compat(self)
    }
}

/// How a node's firmware stands against the host's, from [`Version::compat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compat {
    /// The same version.
    Same,
    /// The same major and minor, an older patch. Driven as usual.
    PatchBehind,
    /// The same major and minor, a newer patch. Driven as usual.
    PatchAhead,
    /// An older major or minor: the node needs an update before this host can drive it.
    NodeBehind,
    /// A newer major or minor: the host needs an update before it can drive the node.
    NodeAhead,
}

impl Compat {
    /// Whether this host reads the node's layouts, so it can be driven.
    #[must_use]
    pub const fn drivable(self) -> bool {
        matches!(self, Self::Same | Self::PatchBehind | Self::PatchAhead)
    }

    /// Whether the node's version is older than the host's.
    #[must_use]
    pub const fn node_behind(self) -> bool {
        matches!(self, Self::PatchBehind | Self::NodeBehind)
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
pub const NODE_VERSION: Version = Version::new(0, 2, 2);
