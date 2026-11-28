//! The [`Ready`] a bridge announces itself with: a frozen core laid out by hand, then the rest
//! in postcard.
//!
//! **The core's layout never changes**, so any host reads any bridge's identity, image, version,
//! trial and uptime, whatever else the two builds disagree on. Little-endian, after the
//! message's code byte:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 1 | `core_len`: the bytes of core that follow, [`READY_CORE_LEN`] in this build |
//! | 1 | 6 | `mac` |
//! | 7 | 32 | `image`: zeros when the P4 cannot read its digest |
//! | 39 | 3 | `version`: major, minor, patch |
//! | 42 | 1 | `trial` ([`Trial`]'s code) |
//! | 43 | 4 | `uptime_ms` |
//! | 1 + `core_len` | rest | [`ReadyRest`], postcard |
//!
//! Fields are appended to the core, never inserted, moved or resized, and `core_len` grows with
//! them, up to 255. A decoder reads the fields it knows, skips to `1 + core_len`, and refuses a
//! `Ready` whose `core_len` is under [`READY_CORE_LEN`] or runs past the body.
//!
//! **The rest is read only on the bridge's major.** Postcard is not self-describing, and another
//! major's bytes can decode as wrong values, so a host on another major ([`BRIDGE_VERSION`])
//! does not try. One on the same major whose rest does not decode is talking to a build that
//! changed a message without the bump the version asks for ([`Standing::OtherBuild`]).
//!
//! The trial's codes are spelled out in `match` arms rather than cast, and are never reused. One
//! this build cannot name is kept as [`Trial::Other`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{LoopPhase, RadioInfo, ShortStr};
use crate::bus::{NodeBoard, Version};
use crate::mac::Mac;
use crate::ota::image::DIGEST_LEN;
use crate::reset::ResetCause;

/// The bytes of core this build writes after `core_len`.
pub const READY_CORE_LEN: usize = 46;

/// The bridge firmware this tree builds, and the version its `Ready` carries.
///
/// Mirrors `version` in `firmware/p4-bridge/Cargo.toml`; `crates/wartui-proto/tests/version.rs`
/// fails when the two differ. It follows semantic versioning for the link's unfrozen messages
/// (`crate::link` `//!`): a changed message is a major, a new one a minor.
pub const BRIDGE_VERSION: Version = Version::new(0, 1, 0);

const MAC: usize = 1;
const IMAGE: usize = MAC + 6;
const VERSION: usize = IMAGE + DIGEST_LEN;
const TRIAL: usize = VERSION + 3;
const UPTIME: usize = TRIAL + 1;
const CORE_END: usize = UPTIME + 4;

const _: () = assert!(CORE_END == 1 + READY_CORE_LEN);

/// Where the booted image's trial stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trial {
    /// Not on trial: code 0.
    Settled,
    /// On trial: code 1. A host frame the P4 decodes, once its C6 has answered, confirms it.
    OnTrial,
    /// On trial, and the confirm failed for this host: code 2.
    ConfirmFailed,
    /// A code this build cannot name.
    Other(u8),
}

impl Trial {
    /// The trial's byte in the core.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Settled => 0,
            Self::OnTrial => 1,
            Self::ConfirmFailed => 2,
            Self::Other(code) => code,
        }
    }

    /// The trial a core's byte names.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Settled,
            1 => Self::OnTrial,
            2 => Self::ConfirmFailed,
            code => Self::Other(code),
        }
    }
}

/// The frozen part of a [`Ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyCore {
    /// The bridge's identity: its C6's MAC, or the P4's own while no C6 answers.
    pub mac: Mac,
    /// The SHA-256 digest appended to the P4's booted image ([`crate::ota::image`]), or zeros
    /// when the P4 cannot read one.
    pub image: [u8; DIGEST_LEN],
    /// The P4 firmware's version.
    pub version: Version,
    /// Where the booted image's trial stands.
    pub trial: Trial,
    /// Milliseconds since this bridge life began. A restart of either chip begins a new life,
    /// so the host tells a new life from a second answer to one `Identify` by it
    /// (`crates/wartui-bridge/src/gate.rs`). It wraps after 49.7 days.
    pub uptime_ms: u32,
}

impl ReadyCore {
    /// The booted image's digest, or `None` when the P4 could not read one.
    #[must_use]
    pub fn image(&self) -> Option<[u8; DIGEST_LEN]> {
        (self.image != [0; DIGEST_LEN]).then_some(self.image)
    }
}

/// What a [`Ready`] carries past its core. Postcard, so its bytes follow the type: within a
/// major they never change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyRest {
    /// The bridge firmware's version text.
    pub fw_version: ShortStr,
    /// Why this life started. Anything but [`ResetCause::PowerOn`] means the bridge restarted
    /// underneath a host that may not have noticed.
    pub reset_cause: ResetCause,
    /// Where the previous life stopped, when the reset preserved it.
    pub last_phase: LoopPhase,
    /// What carries the nodes, which the P4 reads at boot. It sets the slots driven and the
    /// names an operator reads for them.
    pub node_board: NodeBoard,
    /// The ESP32-C6 radio behind the bridge, as the C6 reported it. `None` while the C6 has not
    /// answered.
    pub radio: Option<RadioInfo>,
}

/// The bridge's announcement ([`super::BridgeToHost::Ready`]).
///
/// Laid out by hand ([`Ready::encode`], [`Ready::decode`]), never through postcard: its serde
/// impls refuse, so a postcard encoding of a message cannot carry one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The frozen part, which every host reads.
    pub core: ReadyCore,
    /// The rest, or `None` when this host did not read it: another major, or bytes that did not
    /// decode.
    pub rest: Option<ReadyRest>,
}

impl Ready {
    /// Writes the `Ready` after its code byte into `out`, returning its length, or `None` when
    /// `out` cannot hold it.
    ///
    /// A bridge always has a rest. A `Ready` with none writes its core alone, which reads back
    /// with none.
    #[must_use]
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let core = out.get_mut(..CORE_END)?;
        core[0] = READY_CORE_LEN as u8;
        core[MAC..IMAGE].copy_from_slice(&self.core.mac);
        core[IMAGE..VERSION].copy_from_slice(&self.core.image);
        let Version { major, minor, patch } = self.core.version;
        core[VERSION..TRIAL].copy_from_slice(&[major, minor, patch]);
        core[TRIAL] = self.core.trial.code();
        core[UPTIME..CORE_END].copy_from_slice(&self.core.uptime_ms.to_le_bytes());
        let rest = match &self.rest {
            Some(rest) => postcard::to_slice(rest, &mut out[CORE_END..]).ok()?.len(),
            None => 0,
        };
        Some(CORE_END + rest)
    }

    /// Reads a `Ready` from `body`, the bytes after its code.
    ///
    /// `None` when `core_len` is under [`READY_CORE_LEN`] or runs past `body`. The rest is read
    /// only when the core's major is [`BRIDGE_VERSION`]'s, and is `None` when it does not decode.
    #[must_use]
    pub fn decode(body: &[u8]) -> Option<Self> {
        let (&core_len, _) = body.split_first()?;
        let core_len = usize::from(core_len);
        if core_len < READY_CORE_LEN {
            return None;
        }
        let b = body.get(..1 + core_len)?;
        let core = ReadyCore {
            mac: *b[MAC..IMAGE].first_chunk()?,
            image: *b[IMAGE..VERSION].first_chunk()?,
            version: Version::new(b[VERSION], b[VERSION + 1], b[VERSION + 2]),
            trial: Trial::from_code(b[TRIAL]),
            uptime_ms: u32::from_le_bytes(*b[UPTIME..CORE_END].first_chunk()?),
        };
        let rest = (core.version.major == BRIDGE_VERSION.major)
            .then(|| postcard::from_bytes(&body[1 + core_len..]).ok())
            .flatten();
        Some(Self { core, rest })
    }

    /// How this bridge stands against the host this tree builds ([`Standing::of`]).
    #[must_use]
    pub fn standing(&self) -> Standing {
        Standing::of(self.core.version, BRIDGE_VERSION, self.rest.is_some())
    }
}

impl Serialize for Ready {
    fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("a Ready is laid out by hand, never serialized"))
    }
}

impl<'de> Deserialize<'de> for Ready {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom("a Ready is laid out by hand, never deserialized"))
    }
}

/// One end of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The bridge.
    Bridge,
    /// The host.
    Host,
}

/// How a bridge's version stands against a host's, by the semantic versioning `crate::link`
/// `//!` states. Patches are never compared: no message differs between them.
///
/// A comparison of its own: a node's version is read by [`Version::compat`], the node bus's
/// rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The same major and minor: every message reads.
    Same,
    /// Another minor: every message both know reads, and `older` lacks the other's newer ones.
    Fewer {
        /// The end on the lower minor.
        older: End,
    },
    /// Another major: the two share the frozen core only.
    Apart {
        /// The end on the lower major.
        older: End,
    },
    /// The same major, and a rest that did not decode: a build that changed a message without a
    /// major bump. The two share the frozen core only.
    OtherBuild,
}

impl Standing {
    /// How a bridge on `bridge` stands against a host on `host`, which read the bridge's rest
    /// when `rest_read`.
    #[must_use]
    pub const fn of(bridge: Version, host: Version, rest_read: bool) -> Self {
        let older = if bridge.major < host.major
            || (bridge.major == host.major && bridge.minor < host.minor)
        {
            End::Bridge
        } else {
            End::Host
        };
        if bridge.major != host.major {
            Self::Apart { older }
        } else if !rest_read {
            Self::OtherBuild
        } else if bridge.minor != host.minor {
            Self::Fewer { older }
        } else {
            Self::Same
        }
    }
}
