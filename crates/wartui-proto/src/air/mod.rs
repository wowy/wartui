//! The on-air ESP-NOW frames.
//!
//! Little-endian, no CRC. Encoding and decoding are written out by hand rather
//! than transmuting a `#[repr(packed)]` struct: the byte layout is a contract
//! between two separately compiled programs, so it deserves to be spelled out
//! and tested against real bytes.
//!
//! Every frame in both directions is wartui's own. ESP-NOW has no addressing
//! above the MAC layer and a node broadcasts to `FF:FF:FF:FF:FF:FF`, so anything
//! sharing a format on the control channel is in everybody's conversation at
//! once.
//!
//! A magic of our own solves it. It is checked before anything else, so another
//! firmware's frame costs one `memcmp`. [`foreign`] recognises one such format,
//! `ENOW`, in order to *report* it.
//!
//! The header carries a version. It is the lever held for the first change a
//! fleet in the field has to survive: a node speaking a version this host does
//! not know is counted and named rather than half-decoded. Until wartui 1.0
//! nothing is such a change, so the byte does not move — see
//! [`WIRE_VERSION`].
//!
//! A sighting never travels alone. A node packs every access point or
//! advertiser it has to report into one [`SightingBatch`], filling it to
//! [`SIGHTING_BATCH_MAX`] bytes — ESP-NOW's own payload ceiling — and sends
//! what it has at the end of a dwell or a Bluetooth scan rather than holding
//! any of it for the next one: the host stamps each record's position on
//! arrival, so a sighting held across a dwell would carry the car's next
//! position instead of the one it was heard at. [`SightingMsg`] is what one
//! record of the batch holds; [`SightingBatch::decode`] validates the whole
//! frame — every record within its limits and no trailing bytes — before
//! yielding any of it, the same "never half-decoded" rule every other frame
//! here follows.

use core::fmt;

mod admin;
mod heartbeat;
mod security;
mod sighting;

pub use admin::{ADMIN_FLAG_BLE, ADMIN_MSG_LEN, AdminMsg, CLEAR_MSG_LEN, ClearMsg, wire_epoch};
pub use heartbeat::{
    CAP_FLAG_5G, CAPABILITY_MAJOR, CAPABILITY_MINOR, Capabilities, HEARTBEAT_MSG_LEN, HeartbeatMsg,
};
pub use security::Security;
pub use sighting::{
    EXT_MAX, RecordKind, SIGHTING_BATCH_HEADER, SIGHTING_BATCH_MAX, SIGHTING_RECORD_MAX,
    SIGHTING_RECORD_MIN, SIGHTINGS_PER_BATCH_MAX, SSID_MAX, SightingBatch, SightingBatchWriter,
    SightingMsg,
};

/// Frame preamble, `"WTUI"`. Four raw bytes, not a NUL-terminated string.
pub const MAGIC: [u8; 4] = *b"WTUI";

/// The frame version this build speaks and the only one it decodes.
///
/// 1 until wartui 1.0, whatever the layouts below do. Nothing here is
/// compatible with an earlier wartui and the fleet is flashed together, so a
/// byte telling the two apart marks a difference nothing acts on and costs a
/// re-pin of every fixture in `tests/wire.rs` to say it. This is held for the
/// first change a fleet in the field has to survive; a layout that changes
/// shape before then simply changes shape. Anything else is reported as
/// incompatible rather than guessed at — see the module docs.
pub const WIRE_VERSION: u8 = 1;

/// The channel every node returns to in order to speak to the controller.
///
/// Nothing negotiates this: a node that
/// picked a different one would be transmitting into an empty room.
pub const CONTROL_CHANNEL: u8 = 6;

const OFF_VERSION: usize = 4;
const OFF_TYPE: usize = 5;
const OFF_BODY: usize = 6;

/// The header alone: enough to know whether a frame is ours and what shape it
/// claims to be.
const HEADER_LEN: usize = OFF_BODY;

/// What a frame is, with the direction in the high bit so a misrouted frame is a
/// decode error rather than a plausible one of something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgType {
    /// Node → core, once per completed channel sweep.
    Heartbeat = 0x01,
    /// Node → core, every newly-seen BSSID or advertiser from one dwell or
    /// Bluetooth scan, packed into [`SightingBatch`].
    SightingBatch = 0x02,
    /// Core → node. Carries a channel and Bluetooth assignment.
    Admin = 0x81,
    /// Core → node. Asks the node to forget every address it has reported.
    Clear = 0x82,
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
            0x81 => Ok(Self::Admin),
            0x82 => Ok(Self::Clear),
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
    /// A fixed-length frame that was not its own length, or a [`SightingBatch`]
    /// carrying bytes past its last record.
    ///
    /// [`HeartbeatMsg`] and [`AdminMsg`] are each exactly one size, so anything
    /// else carrying their type byte came from a build whose layout differs from
    /// this one's. Longer is the dangerous half: the leading bytes would parse,
    /// and the frame would be adopted as a plausible wrong assignment. Since
    /// [`WIRE_VERSION`] does not move before 1.0, this is what says a fleet is
    /// half-way through a reflash. A batch is not fixed-length, but `count`
    /// records fully accounts for its bytes or it is the same fault: a frame
    /// claiming to be something this build's layout is not.
    BadLength {
        /// Bytes this build's layout is.
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// First four bytes were not [`MAGIC`].
    BadMagic,
    /// A frame of ours, from a build speaking a version this one does not.
    /// Reported rather than guessed at: the whole point of the version byte is
    /// that a layout change must not decode as a plausible older frame.
    BadVersion(u8),
    /// Type byte named no frame this build knows.
    UnknownType(u8),
    /// `ssid_len` exceeded [`SSID_MAX`], which 802.11 makes impossible, so the
    /// frame is malformed rather than merely unusual.
    SsidTooLong(u8),
    /// `ext_len` exceeded [`EXT_MAX`], which no parser this build knows will
    /// produce, so the frame is malformed rather than merely unusual.
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
/// Every decoder starts here, so magic, version and type are rejected in the
/// same order and with the same errors wherever a frame arrives.
fn header(buf: &[u8]) -> Result<MsgType, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::TooShort { need: HEADER_LEN, got: buf.len() });
    }
    if buf[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if buf[OFF_VERSION] != WIRE_VERSION {
        return Err(DecodeError::BadVersion(buf[OFF_VERSION]));
    }
    MsgType::try_from(buf[OFF_TYPE])
}

/// Write the header for `msg_type` into the front of `out`.
fn write_header(out: &mut [u8], msg_type: MsgType) {
    out[..4].copy_from_slice(&MAGIC);
    out[OFF_VERSION] = WIRE_VERSION;
    out[OFF_TYPE] = msg_type.as_u8();
}

/// Any frame, dispatched on the type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// Node → core.
    Heartbeat(HeartbeatMsg),
    /// Node → core.
    Sightings(SightingBatch<'a>),
    /// Core → node. Seeing one this host did not send means another core is
    /// driving this fleet.
    Admin(AdminMsg),
    /// Core → node. Seeing one this host did not send means another core is
    /// driving this fleet, the same as [`Frame::Admin`].
    Clear(ClearMsg),
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
        }
    }
}

/// Recognising the vendor's traffic, in order to report it.
///
/// Nothing here decodes a byte. The fields are another fleet's idea of another
/// fleet and acting on them would be adopting it. But a vendor core or node on
/// the control channel is an operational fact — it is transmitting where these
/// nodes are listening, and on a stock fleet the probe requests are active
/// scans — so counting it as line noise would hide the one clue an operator has
/// for a channel that is busier than the fleet can explain.
///
/// The vendor magic is all that is matched. wartui's own frames carry [`MAGIC`],
/// so anything carrying `ENOW` belongs to somebody else by definition.
pub mod foreign;
