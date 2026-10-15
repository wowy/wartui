//! The on-air ESP-NOW frames.
//!
//! Little-endian, no CRC. Encoding and decoding are written out by hand rather than
//! transmuting a `#[repr(packed)]` struct. The byte layout is a contract between separately
//! compiled programs, so it is spelled out and tested against real bytes.
//!
//! Every frame in both directions is wartui's own. ESP-NOW has no addressing above the MAC
//! layer, and a node's heartbeat broadcasts to `FF:FF:FF:FF:FF:FF`. Anything sharing a
//! format on the control channel is in everybody's conversation at once.
//!
//! A magic of our own solves that. It is checked before anything else, so another
//! firmware's frame costs one `memcmp`. [`foreign`] recognizes one such format, `ENOW`,
//! to *report* it.
//!
//! The header carries a version byte. A frame of ours with a version this build does not
//! know is counted and named, never half-decoded. The byte does not move before 1.0 (see
//! [`WIRE_VERSION`]).
//!
//! One part of the wire is frozen until 1.0, the single exception to flashing the fleet
//! together: the frame header, the heartbeat's type byte and the first
//! [`HEARTBEAT_PREFIX_LEN`] bytes of its body, which carry the node's [`Version`], [`Board`]
//! and flags, and the update frames ([`OtaBeginMsg`], [`OtaChunkMsg`], [`OtaCommitMsg`],
//! [`OtaAbortMsg`], [`OtaConfirmMsg`] and [`OtaStatusMsg`]), type bytes and layouts both.
//! Everything else may change with a minor version. A host reads that prefix from a
//! heartbeat of any length ([`Capabilities::decode_prefix`]), so a node a minor version
//! behind is named and marked for an update rather than lost among undecodable frames, and
//! the update frames are how it then updates that node.
//!
//! A node packs a dwell's or Bluetooth scan's sightings into [`SightingBatch`]es of up to
//! [`SIGHTING_BATCH_MAX`] bytes. A full batch goes out at once and a new one starts. What
//! is left goes out when the dwell or scan ends, so nothing is held past it. The host
//! stamps a position on each record as it arrives, so a held sighting would get the wrong
//! position. [`SightingBatch::decode`] checks the whole frame
//! before it yields any record.

use core::fmt;

mod admin;
mod health;
mod heartbeat;
mod ota;
mod security;
mod sighting;
mod version;

pub use admin::{ADMIN_FLAG_BLE, ADMIN_MSG_LEN, AdminMsg, CLEAR_MSG_LEN, ClearMsg, wire_epoch};
pub use health::{DieTemp, Vdd};
pub use heartbeat::{
    Board, CAP_FLAG_5G, CAP_FLAG_TRIAL, Capabilities, HEARTBEAT_MSG_LEN, HEARTBEAT_PREFIX_LEN,
    HeartbeatMsg,
};
pub use ota::{
    OTA_ABORT_MSG_LEN, OTA_BEGIN_MSG_LEN, OTA_CHUNK, OTA_CHUNK_HEADER, OTA_CHUNK_MSG_MAX,
    OTA_COMMIT_MSG_LEN, OTA_CONFIRM_MSG_LEN, OTA_STATUS_MSG_LEN, OtaAbortMsg, OtaBeginMsg,
    OtaChunkMsg, OtaCommitMsg, OtaConfirmMsg, OtaState, OtaStatusMsg,
};
pub use security::Security;
pub use sighting::{
    EXT_MAX, RecordKind, SIGHTING_BATCH_HEADER, SIGHTING_BATCH_MAX, SIGHTING_RECORD_MAX,
    SIGHTING_RECORD_MIN, SIGHTINGS_PER_BATCH_MAX, SSID_MAX, SightingBatch, SightingBatchWriter,
    SightingMsg,
};
pub use version::{Compat, NODE_VERSION, Version};

/// Frame preamble, `"WTUI"`. Four raw bytes, not a NUL-terminated string.
pub const MAGIC: [u8; 4] = *b"WTUI";

/// The frame version this build speaks and the only one it decodes.
///
/// 1 until wartui 1.0, whatever the layouts do. It is held for the first change a fleet
/// in the field has to survive. Before then the fleet is flashed together, and a layout
/// simply changes shape. `AGENTS.md` § "Invariants that are easy to break" gives the
/// policy.
pub const WIRE_VERSION: u8 = 1;

/// The channel every air frame is sent on, and every node returns to between sweeps.
pub const CONTROL_CHANNEL: u8 = 6;

const OFFSET_VERSION: usize = 4;
const OFFSET_TYPE: usize = 5;
const OFFSET_BODY: usize = 6;
const HEADER_LEN: usize = OFFSET_BODY;

/// The message type. The high bit is the transmission direction, so a misrouted frame is a
/// decode error rather than a plausible frame of another type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgType {
    /// Node → host: [`HeartbeatMsg`].
    Heartbeat = 0x01,
    /// Node → host: [`SightingBatch`].
    SightingBatch = 0x02,
    /// Node → host: [`OtaStatusMsg`].
    OtaStatus = 0x03,
    /// Host → node: [`AdminMsg`].
    Admin = 0x81,
    /// Host → node: [`ClearMsg`].
    Clear = 0x82,
    /// Host → node: [`OtaBeginMsg`].
    OtaBegin = 0x83,
    /// Host → node: [`OtaChunkMsg`].
    OtaChunk = 0x84,
    /// Host → node: [`OtaCommitMsg`].
    OtaCommit = 0x85,
    /// Host → node: [`OtaAbortMsg`].
    OtaAbort = 0x86,
    /// Host → node: [`OtaConfirmMsg`].
    OtaConfirm = 0x87,
}

impl MsgType {
    /// The discriminant as it appears on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for MsgType {
    type Error = DecodeError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0x01 => Ok(Self::Heartbeat),
            0x02 => Ok(Self::SightingBatch),
            0x03 => Ok(Self::OtaStatus),
            0x81 => Ok(Self::Admin),
            0x82 => Ok(Self::Clear),
            0x83 => Ok(Self::OtaBegin),
            0x84 => Ok(Self::OtaChunk),
            0x85 => Ok(Self::OtaCommit),
            0x86 => Ok(Self::OtaAbort),
            0x87 => Ok(Self::OtaConfirm),
            other => Err(DecodeError::UnknownType(other)),
        }
    }
}

/// Why a byte slice was not a valid frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the message type requires.
    TooShort {
        /// Bytes required for this message type.
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// A fixed-length frame that was not its own length or a [`SightingBatch`] with
    /// bytes past its last record.
    ///
    /// Either way, the frame came from a build whose layout differs from this one's.
    /// Since [`WIRE_VERSION`] does not move before 1.0, this is what says a fleet is
    /// half-way through a reflash. Longer is the dangerous case: the leading bytes would
    /// parse, and the frame would be adopted as a plausible wrong assignment.
    BadLength {
        /// Bytes this build's layout is.
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// The first four bytes were not [`MAGIC`].
    BadMagic,
    /// A frame of ours, from a build speaking a version this one does not. Reported,
    /// never guessed at: a layout change must not decode as a plausible frame.
    BadVersion(u8),
    /// Type byte named no frame this build knows.
    UnknownType(u8),
    /// `ssid_len` exceeded [`SSID_MAX`]. 802.11 makes that impossible, so the frame is
    /// malformed.
    SsidTooLong(u8),
    /// `ext_len` exceeded [`EXT_MAX`]. No parser in this build produces that, so the
    /// frame is malformed.
    ExtTooLong(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { need, got } => write!(f, "frame too short: need {need}, got {got}"),
            Self::BadLength { need, got } => write!(f, "frame is {got} bytes, not {need}"),
            Self::BadMagic => f.write_str("bad magic, expected \"WTUI\""),
            Self::BadVersion(v) => write!(f, "wire version {v}, expected {WIRE_VERSION}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:#04x}"),
            Self::SsidTooLong(n) => write!(f, "ssid length {n} exceeds {SSID_MAX}"),
            Self::ExtTooLong(n) => write!(f, "ext length {n} exceeds {EXT_MAX}"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Check the header and return the type it names.
///
/// Every decoder starts here, so magic, version, and type are rejected in the same order
/// and with the same errors wherever a frame arrives.
fn header(buf: &[u8]) -> Result<MsgType, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::TooShort { need: HEADER_LEN, got: buf.len() });
    }
    if buf[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if buf[OFFSET_VERSION] != WIRE_VERSION {
        return Err(DecodeError::BadVersion(buf[OFFSET_VERSION]));
    }
    MsgType::try_from(buf[OFFSET_TYPE])
}

/// Write the header for `msg_type` into the front of `out`.
fn write_header(out: &mut [u8], msg_type: MsgType) {
    out[..4].copy_from_slice(&MAGIC);
    out[OFFSET_VERSION] = WIRE_VERSION;
    out[OFFSET_TYPE] = msg_type.as_u8();
}

/// Any of our frames. The type byte in the header picks the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// Node → host.
    Heartbeat(HeartbeatMsg),
    /// Node → host.
    Sightings(SightingBatch<'a>),
    /// Host → node. One this host did not send means another host is driving the fleet.
    Admin(AdminMsg),
    /// Host → node. One this host did not send means the same as for [`Frame::Admin`].
    Clear(ClearMsg),
    /// Host → node: open an update session.
    OtaBegin(OtaBeginMsg),
    /// Host → node: image bytes.
    OtaChunk(OtaChunkMsg<'a>),
    /// Host → node: check the image.
    OtaCommit(OtaCommitMsg),
    /// Host → node: drop the session.
    OtaAbort(OtaAbortMsg),
    /// Host → node: keep the image on trial.
    OtaConfirm(OtaConfirmMsg),
    /// Node → host: where an update session stands.
    OtaStatus(OtaStatusMsg),
}

impl<'a> Frame<'a> {
    /// Decode any frame, choosing the layout from the type byte.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Heartbeat => HeartbeatMsg::decode(buf).map(Frame::Heartbeat),
            MsgType::SightingBatch => SightingBatch::decode(buf).map(Frame::Sightings),
            MsgType::Admin => AdminMsg::decode(buf).map(Frame::Admin),
            MsgType::Clear => ClearMsg::decode(buf).map(Frame::Clear),
            MsgType::OtaBegin => OtaBeginMsg::decode(buf).map(Frame::OtaBegin),
            MsgType::OtaChunk => OtaChunkMsg::decode(buf).map(Frame::OtaChunk),
            MsgType::OtaCommit => OtaCommitMsg::decode(buf).map(Frame::OtaCommit),
            MsgType::OtaAbort => OtaAbortMsg::decode(buf).map(Frame::OtaAbort),
            MsgType::OtaConfirm => OtaConfirmMsg::decode(buf).map(Frame::OtaConfirm),
            MsgType::OtaStatus => OtaStatusMsg::decode(buf).map(Frame::OtaStatus),
        }
    }
}

/// Recognizing foreign traffic, to report it.
///
/// Nothing here decodes a byte: acting on another fleet's fields would be adopting
/// them. But another host or node on the control channel transmits where these nodes
/// listen, and other fleet's nodes scan actively. Counting it as line noise would hide
/// the one clue an operator has for a channel busier than the fleet explains.
///
/// Only the vendor magic is matched. wartui's own frames carry [`MAGIC`], so anything
/// carrying `ENOW` is somebody else's.
pub mod foreign;
