//! The messages records carry, and the type bytes that name them.
//!
//! Little-endian, encoded and decoded by hand rather than by transmuting a `#[repr(packed)]`
//! struct, since the layout is a contract between separately compiled programs. A record has no
//! header of its own: the transfer carries magic, version and CRC, and the bus is
//! point-to-point. Each direction numbers its types from 1. A message is decoded whole or
//! refused, never half-decoded.
//!
//! | Up, node → P4 | | Down, P4 → node | |
//! |---|---|---|---|
//! | 1 | [`Heartbeat`] | 1 | [`Assign`] |
//! | 2 | [`SightingBatch`] | 2 | [`Clear`] |
//! | 3 | [`UpdateStatus`] | 3 | [`UpdateBegin`] |
//! | | | 4 | [`UpdateChunk`] |
//! | | | 5 | [`UpdateCommit`] |
//! | | | 6 | [`UpdateAbort`] |
//! | | | 7 | [`UpdateConfirm`] |
//!
//! A node packs a dwell's or Bluetooth scan's sightings into [`SightingBatch`]es of up to
//! [`SIGHTING_BATCH_MAX`](super::SIGHTING_BATCH_MAX) bytes. A full batch goes up at once and a
//! new one starts. What is left goes up when the dwell or scan ends, so nothing is held past it:
//! the host stamps a position on each record as it arrives, so a held sighting would get the
//! wrong position.

use core::fmt;

use super::{
    Assign, Clear, EXT_MAX, Heartbeat, SSID_MAX, SightingBatch, UpdateAbort, UpdateBegin,
    UpdateChunk, UpdateCommit, UpdateConfirm, UpdateStatus,
};

/// The type of a record a node sends.
///
/// [`Heartbeat`](Self::Heartbeat) and [`UpdateStatus`](Self::UpdateStatus) are in the bus's
/// frozen core ([`crate::bus`]): their codes never change, and no code is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UpType {
    /// [`Heartbeat`].
    Heartbeat = 1,
    /// [`SightingBatch`].
    Sightings = 2,
    /// [`UpdateStatus`].
    UpdateStatus = 3,
}

impl UpType {
    /// The type byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The type a byte names, if this build knows it.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Heartbeat),
            2 => Some(Self::Sightings),
            3 => Some(Self::UpdateStatus),
            _ => None,
        }
    }
}

/// The type of a record the P4 sends a node.
///
/// [`UpdateBegin`](Self::UpdateBegin) to [`UpdateConfirm`](Self::UpdateConfirm) are in the
/// bus's frozen core ([`crate::bus`]): their codes never change, and no code is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DownType {
    /// [`Assign`].
    Assign = 1,
    /// [`Clear`].
    Clear = 2,
    /// [`UpdateBegin`].
    UpdateBegin = 3,
    /// [`UpdateChunk`].
    UpdateChunk = 4,
    /// [`UpdateCommit`].
    UpdateCommit = 5,
    /// [`UpdateAbort`].
    UpdateAbort = 6,
    /// [`UpdateConfirm`].
    UpdateConfirm = 7,
}

impl DownType {
    /// The type byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The type a byte names, if this build knows it.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Assign),
            2 => Some(Self::Clear),
            3 => Some(Self::UpdateBegin),
            4 => Some(Self::UpdateChunk),
            5 => Some(Self::UpdateCommit),
            6 => Some(Self::UpdateAbort),
            7 => Some(Self::UpdateConfirm),
            _ => None,
        }
    }
}

/// Why a body was not a valid message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the message requires.
    TooShort {
        /// Bytes required.
        need: usize,
        /// Bytes present.
        got: usize,
    },
    /// A fixed-length body that was not its own length, or a [`SightingBatch`] with bytes past
    /// its last record.
    ///
    /// Either way, the body came from a build whose layout differs from this one's: a fleet
    /// half-way through a reflash. Longer is the dangerous case: the leading bytes would parse,
    /// and the body would be taken as a plausible wrong message.
    BadLength {
        /// Bytes this build's layout is.
        need: usize,
        /// Bytes present.
        got: usize,
    },
    /// A type byte, or a sighting record's kind, this build does not know.
    UnknownType(u8),
    /// `ssid_len` exceeded [`SSID_MAX`]. 802.11 makes that impossible, so the body is
    /// malformed.
    SsidTooLong(u8),
    /// `ext_len` exceeded [`EXT_MAX`]. No parser in this build produces that, so the body is
    /// malformed.
    ExtTooLong(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { need, got } => write!(f, "body too short: need {need}, got {got}"),
            Self::BadLength { need, got } => write!(f, "body is {got} bytes, not {need}"),
            Self::UnknownType(t) => write!(f, "unknown type {t:#04x}"),
            Self::SsidTooLong(n) => write!(f, "ssid length {n} exceeds {SSID_MAX}"),
            Self::ExtTooLong(n) => write!(f, "ext length {n} exceeds {EXT_MAX}"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Check a fixed-length body is exactly `len` bytes, which is every fixed message's whole
/// validation.
pub(super) fn fixed(body: &[u8], len: usize) -> Result<(), DecodeError> {
    if body.len() == len {
        Ok(())
    } else {
        Err(DecodeError::BadLength { need: len, got: body.len() })
    }
}

/// The little-endian `u32` at `at`.
pub(super) fn le_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Any message a node sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpMsg<'a> {
    /// The node's liveness.
    Heartbeat(Heartbeat),
    /// What the node heard.
    Sightings(SightingBatch<'a>),
    /// Where an update session stands.
    UpdateStatus(UpdateStatus),
}

impl<'a> UpMsg<'a> {
    /// Decode the body of a record of type `kind`.
    ///
    /// # Errors
    /// [`DecodeError::UnknownType`] for a type this build does not know, and see
    /// [`DecodeError`].
    pub fn decode(kind: u8, body: &'a [u8]) -> Result<Self, DecodeError> {
        match UpType::from_u8(kind).ok_or(DecodeError::UnknownType(kind))? {
            UpType::Heartbeat => Heartbeat::decode(body).map(Self::Heartbeat),
            UpType::Sightings => SightingBatch::decode(body).map(Self::Sightings),
            UpType::UpdateStatus => UpdateStatus::decode(body).map(Self::UpdateStatus),
        }
    }

    /// The record type that carries it.
    #[must_use]
    pub const fn kind(&self) -> UpType {
        match self {
            Self::Heartbeat(_) => UpType::Heartbeat,
            Self::Sightings(_) => UpType::Sightings,
            Self::UpdateStatus(_) => UpType::UpdateStatus,
        }
    }
}

/// Any message the P4 sends a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownMsg<'a> {
    /// A channel and Bluetooth assignment.
    Assign(Assign),
    /// Forget every reported address.
    Clear(Clear),
    /// Open an update session.
    UpdateBegin(UpdateBegin),
    /// Image bytes.
    UpdateChunk(UpdateChunk<'a>),
    /// Check the image.
    UpdateCommit(UpdateCommit),
    /// Drop the session.
    UpdateAbort(UpdateAbort),
    /// Confirm the image on trial, ending its trial.
    UpdateConfirm(UpdateConfirm),
}

impl<'a> DownMsg<'a> {
    /// Decode the body of a record of type `kind`.
    ///
    /// # Errors
    /// [`DecodeError::UnknownType`] for a type this build does not know, and see
    /// [`DecodeError`].
    pub fn decode(kind: u8, body: &'a [u8]) -> Result<Self, DecodeError> {
        match DownType::from_u8(kind).ok_or(DecodeError::UnknownType(kind))? {
            DownType::Assign => Assign::decode(body).map(Self::Assign),
            DownType::Clear => Clear::decode(body).map(Self::Clear),
            DownType::UpdateBegin => UpdateBegin::decode(body).map(Self::UpdateBegin),
            DownType::UpdateChunk => UpdateChunk::decode(body).map(Self::UpdateChunk),
            DownType::UpdateCommit => UpdateCommit::decode(body).map(Self::UpdateCommit),
            DownType::UpdateAbort => UpdateAbort::decode(body).map(Self::UpdateAbort),
            DownType::UpdateConfirm => UpdateConfirm::decode(body).map(Self::UpdateConfirm),
        }
    }

    /// The record type that carries it.
    #[must_use]
    pub const fn kind(&self) -> DownType {
        match self {
            Self::Assign(_) => DownType::Assign,
            Self::Clear(_) => DownType::Clear,
            Self::UpdateBegin(_) => DownType::UpdateBegin,
            Self::UpdateChunk(_) => DownType::UpdateChunk,
            Self::UpdateCommit(_) => DownType::UpdateCommit,
            Self::UpdateAbort(_) => DownType::UpdateAbort,
            Self::UpdateConfirm(_) => DownType::UpdateConfirm,
        }
    }

    /// Write the body into `out` and return its length, or `None` when `out` is too short or a
    /// chunk's data is empty or past [`UPDATE_CHUNK`](super::UPDATE_CHUNK).
    #[must_use]
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        fn put(out: &mut [u8], body: &[u8]) -> Option<usize> {
            out.get_mut(..body.len())?.copy_from_slice(body);
            Some(body.len())
        }
        match self {
            Self::Assign(msg) => put(out, &msg.encode()),
            Self::Clear(msg) => put(out, &msg.encode()),
            Self::UpdateBegin(msg) => put(out, &msg.encode()),
            Self::UpdateChunk(msg) => msg.encode_into(out),
            Self::UpdateCommit(msg) => put(out, &msg.encode()),
            Self::UpdateAbort(msg) => put(out, &msg.encode()),
            Self::UpdateConfirm(msg) => put(out, &msg.encode()),
        }
    }
}
