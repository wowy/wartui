//! The over-the-air update frames.
//!
//! Every frame here is part of the frozen core ([`crate::air`]): its type byte and layout
//! never change before 1.0. A host updates a node on any minor version through them, so
//! a node that cannot read today's assignments can still be brought up to date.
//!
//! The host sends an image as [`OtaChunkMsg`]s of [`OTA_CHUNK`] bytes, which only ESP-NOW
//! v2 carries. The node answers in [`OtaStatusMsg`]s, which stay within v1's 250 bytes like
//! everything else a node sends. [`crate::ota`] is the node's half of the conversation.

use super::{
    Board, DecodeError, MsgType, OFFSET_BODY, Version, fixed, header, le_u32, write_header,
};
use crate::link::{MAX_ESPNOW_PAYLOAD, MAX_ESPNOW_V2_PAYLOAD};

/// Image bytes in every [`OtaChunkMsg`] but the last.
pub const OTA_CHUNK: usize = 1024;

/// Length of [`OtaBeginMsg`] on the wire.
pub const OTA_BEGIN_MSG_LEN: usize = OFFSET_BODY + 1 + 4 + 32 + 3 + 1;

/// Length of an [`OtaChunkMsg`] before its data: the header, session and offset.
pub const OTA_CHUNK_HEADER: usize = OFFSET_BODY + 1 + 4;

/// Length of the longest [`OtaChunkMsg`] on the wire: a full [`OTA_CHUNK`].
pub const OTA_CHUNK_MSG_MAX: usize = OTA_CHUNK_HEADER + OTA_CHUNK;

/// Length of [`OtaCommitMsg`] on the wire.
pub const OTA_COMMIT_MSG_LEN: usize = OFFSET_BODY + 1;

/// Length of [`OtaAbortMsg`] on the wire.
pub const OTA_ABORT_MSG_LEN: usize = OFFSET_BODY + 1;

/// Length of [`OtaConfirmMsg`] on the wire: the header, and nothing else.
pub const OTA_CONFIRM_MSG_LEN: usize = OFFSET_BODY;

/// Length of [`OtaStatusMsg`] on the wire.
pub const OTA_STATUS_MSG_LEN: usize = OFFSET_BODY + 1 + 1 + 4;

const _: () = {
    assert!(OTA_CHUNK_MSG_MAX <= MAX_ESPNOW_V2_PAYLOAD);
    assert!(OTA_STATUS_MSG_LEN <= MAX_ESPNOW_PAYLOAD);
};

/// Host → node: open an update session for one image.
///
/// Forty-seven bytes: the header, then [`session`](Self::session), [`len`](Self::len), the
/// image's SHA-256, the [`Version`] it carries and the [`Board`] it was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaBeginMsg {
    /// The host's name for this session. Every later frame of the session carries it.
    pub session: u8,
    /// Image length in bytes.
    pub len: u32,
    /// SHA-256 of the whole image.
    pub sha256: [u8; 32],
    /// The firmware version the image carries.
    pub version: Version,
    /// The board the image was built for.
    pub board: Board,
}

impl OtaBeginMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, MsgType::OtaBegin, OTA_BEGIN_MSG_LEN)?;
        const B: usize = OFFSET_BODY;
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&buf[B + 5..B + 37]);
        Ok(Self {
            session: buf[B],
            len: le_u32(buf, B + 1),
            sha256,
            version: Version::new(buf[B + 37], buf[B + 38], buf[B + 39]),
            board: Board::from_u8(buf[B + 40]),
        })
    }

    /// Encode to the [`OTA_BEGIN_MSG_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; OTA_BEGIN_MSG_LEN] {
        const B: usize = OFFSET_BODY;
        let mut out = [0u8; OTA_BEGIN_MSG_LEN];
        write_header(&mut out, MsgType::OtaBegin);
        out[B] = self.session;
        out[B + 1..B + 5].copy_from_slice(&self.len.to_le_bytes());
        out[B + 5..B + 37].copy_from_slice(&self.sha256);
        out[B + 37] = self.version.major;
        out[B + 38] = self.version.minor;
        out[B + 39] = self.version.patch;
        out[B + 40] = self.board.as_u8();
        out
    }
}

/// Host → node: image bytes at an offset.
///
/// The header, [`session`](Self::session), [`offset`](Self::offset), then 1 to
/// [`OTA_CHUNK`] bytes of data running to the end of the frame. The data is borrowed from
/// the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaChunkMsg<'a> {
    /// The session this chunk belongs to.
    pub session: u8,
    /// Where in the image `data` starts.
    pub offset: u32,
    /// Image bytes, 1 to [`OTA_CHUNK`] of them.
    pub data: &'a [u8],
}

impl<'a> OtaChunkMsg<'a> {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// [`DecodeError::TooShort`] for a frame with no data, [`DecodeError::BadLength`] for
    /// more than [`OTA_CHUNK`] bytes of it, and see [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::OtaChunk => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() <= OTA_CHUNK_HEADER {
            return Err(DecodeError::TooShort { need: OTA_CHUNK_HEADER + 1, got: buf.len() });
        }
        if buf.len() > OTA_CHUNK_MSG_MAX {
            return Err(DecodeError::BadLength { need: OTA_CHUNK_MSG_MAX, got: buf.len() });
        }
        Ok(Self {
            session: buf[OFFSET_BODY],
            offset: le_u32(buf, OFFSET_BODY + 1),
            data: &buf[OTA_CHUNK_HEADER..],
        })
    }

    /// Write the frame into `out`, returning how many bytes it took.
    ///
    /// `None` if `data` is empty or longer than [`OTA_CHUNK`], or `out` is too small;
    /// [`OTA_CHUNK_MSG_MAX`] is always enough. Nothing is written when it fails.
    #[must_use]
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        if self.data.is_empty() || self.data.len() > OTA_CHUNK {
            return None;
        }
        let len = OTA_CHUNK_HEADER + self.data.len();
        let out = out.get_mut(..len)?;
        write_header(out, MsgType::OtaChunk);
        out[OFFSET_BODY] = self.session;
        out[OFFSET_BODY + 1..OTA_CHUNK_HEADER].copy_from_slice(&self.offset.to_le_bytes());
        out[OTA_CHUNK_HEADER..].copy_from_slice(self.data);
        Some(len)
    }
}

/// Host → node: every chunk is sent; check the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaCommitMsg {
    /// The session to check.
    pub session: u8,
}

impl OtaCommitMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, MsgType::OtaCommit, OTA_COMMIT_MSG_LEN)?;
        Ok(Self { session: buf[OFFSET_BODY] })
    }

    /// Encode to the [`OTA_COMMIT_MSG_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; OTA_COMMIT_MSG_LEN] {
        let mut out = [0u8; OTA_COMMIT_MSG_LEN];
        write_header(&mut out, MsgType::OtaCommit);
        out[OFFSET_BODY] = self.session;
        out
    }
}

/// Host → node: drop the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaAbortMsg {
    /// The session to drop. Any other session is left alone.
    pub session: u8,
}

impl OtaAbortMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, MsgType::OtaAbort, OTA_ABORT_MSG_LEN)?;
        Ok(Self { session: buf[OFFSET_BODY] })
    }

    /// Encode to the [`OTA_ABORT_MSG_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; OTA_ABORT_MSG_LEN] {
        let mut out = [0u8; OTA_ABORT_MSG_LEN];
        write_header(&mut out, MsgType::OtaAbort);
        out[OFFSET_BODY] = self.session;
        out
    }
}

/// Host → node: confirm the image on trial, ending its trial.
///
/// Header only. A node runs a fresh image on trial ([`CAP_FLAG_TRIAL`]) until it hears
/// the host; this is the frame an update sends to say so. It names no session, because
/// the session ended with the reboot.
///
/// [`CAP_FLAG_TRIAL`]: super::CAP_FLAG_TRIAL
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaConfirmMsg;

impl OtaConfirmMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, MsgType::OtaConfirm, OTA_CONFIRM_MSG_LEN)?;
        Ok(Self)
    }

    /// Encode to the [`OTA_CONFIRM_MSG_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; OTA_CONFIRM_MSG_LEN] {
        let mut out = [0u8; OTA_CONFIRM_MSG_LEN];
        write_header(&mut out, MsgType::OtaConfirm);
        out
    }
}

/// Where a node's update session stands, as [`OtaStatusMsg`] carries it.
///
/// Each byte is fixed once assigned. A byte this build has no name for decodes to
/// [`OtaState::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OtaState {
    /// No session, or the session named is not the current one: byte 0.
    Idle,
    /// Taking chunks; `next_offset` is the next one wanted: byte 1.
    Receiving,
    /// The hash matched, and the node is about to switch slots and reboot: byte 2.
    Verified,
    /// Every byte arrived and the hash did not match: byte 3.
    HashMismatch,
    /// The image was built for another board: byte 4.
    WrongBoard,
    /// The image is empty or larger than the slot: byte 5.
    TooLarge,
    /// Erasing or writing the slot failed: byte 6.
    FlashError,
    /// A byte this build does not name.
    Unknown(u8),
}

impl OtaState {
    /// The byte the status carries.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Receiving => 1,
            Self::Verified => 2,
            Self::HashMismatch => 3,
            Self::WrongBoard => 4,
            Self::TooLarge => 5,
            Self::FlashError => 6,
            Self::Unknown(byte) => byte,
        }
    }

    /// Read the status's state byte. Never fails.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Self {
        match byte {
            0 => Self::Idle,
            1 => Self::Receiving,
            2 => Self::Verified,
            3 => Self::HashMismatch,
            4 => Self::WrongBoard,
            5 => Self::TooLarge,
            6 => Self::FlashError,
            other => Self::Unknown(other),
        }
    }
}

/// Node → host: where an update session stands.
///
/// Twelve bytes: the header, [`session`](Self::session), [`state`](Self::state) and
/// [`next_offset`](Self::next_offset). The host's flow control rides on `next_offset`
/// ([`crate::ota`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaStatusMsg {
    /// The session the status answers.
    pub session: u8,
    /// Where it stands.
    pub state: OtaState,
    /// The image offset the node wants next. 0 when there is no session.
    pub next_offset: u32,
}

impl OtaStatusMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, MsgType::OtaStatus, OTA_STATUS_MSG_LEN)?;
        Ok(Self {
            session: buf[OFFSET_BODY],
            state: OtaState::from_u8(buf[OFFSET_BODY + 1]),
            next_offset: le_u32(buf, OFFSET_BODY + 2),
        })
    }

    /// Encode to the [`OTA_STATUS_MSG_LEN`] bytes a host expects.
    #[must_use]
    pub fn encode(&self) -> [u8; OTA_STATUS_MSG_LEN] {
        let mut out = [0u8; OTA_STATUS_MSG_LEN];
        write_header(&mut out, MsgType::OtaStatus);
        out[OFFSET_BODY] = self.session;
        out[OFFSET_BODY + 1] = self.state.as_u8();
        out[OFFSET_BODY + 2..].copy_from_slice(&self.next_offset.to_le_bytes());
        out
    }
}
