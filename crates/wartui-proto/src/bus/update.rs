//! The update messages: the P4 relays the host's image to a node as [`UpdateChunk`]s of
//! [`UPDATE_CHUNK`] bytes, and the node answers in [`UpdateStatus`]es. [`crate::ota`] is the
//! node's half of the conversation.
//!
//! These records, their layouts, the [`UpdateState`] codes and [`UPDATE_CHUNK`] are in the bus's
//! frozen core ([`crate::bus`]), so any later host can update any node build.

use super::msg::{fixed, le_u32};
use super::{Board, DecodeError, Version};
use crate::link::{MAX_NODE_DOWN, MAX_NODE_PAYLOAD};

/// Image bytes in every [`UpdateChunk`] but the last.
pub const UPDATE_CHUNK: usize = 1024;

/// Length of an [`UpdateBegin`] body.
pub const UPDATE_BEGIN_LEN: usize = 1 + 4 + 32 + 3 + 1;

/// Length of an [`UpdateChunk`] before its data: session and offset.
pub const UPDATE_CHUNK_HEADER: usize = 1 + 4;

/// Length of the longest [`UpdateChunk`] body: a full [`UPDATE_CHUNK`].
pub const UPDATE_CHUNK_MAX: usize = UPDATE_CHUNK_HEADER + UPDATE_CHUNK;

/// Length of an [`UpdateCommit`] body.
pub const UPDATE_COMMIT_LEN: usize = 1;

/// Length of an [`UpdateAbort`] body.
pub const UPDATE_ABORT_LEN: usize = 1;

/// Length of an [`UpdateConfirm`] body: nothing.
pub const UPDATE_CONFIRM_LEN: usize = 0;

/// Length of an [`UpdateStatus`] body.
pub const UPDATE_STATUS_LEN: usize = 1 + 1 + 4;

const _: () = {
    assert!(UPDATE_CHUNK_MAX == MAX_NODE_DOWN, "a full chunk is the longest down body");
    assert!(UPDATE_STATUS_LEN <= MAX_NODE_PAYLOAD);
};

/// P4 → node: open an update session for one image.
///
/// Forty-one bytes: [`session`](Self::session), [`len`](Self::len), the
/// image's SHA-256, the [`Version`] it carries and the [`Board`] it was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateBegin {
    /// The host's name for this session. Every later message of the session carries it.
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

impl UpdateBegin {
    /// Decode a body.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, UPDATE_BEGIN_LEN)?;
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&buf[5..37]);
        Ok(Self {
            session: buf[0],
            len: le_u32(buf, 1),
            sha256,
            version: Version::new(buf[37], buf[38], buf[39]),
            board: Board::from_u8(buf[40]),
        })
    }

    /// Encode to the [`UPDATE_BEGIN_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; UPDATE_BEGIN_LEN] {
        let mut out = [0u8; UPDATE_BEGIN_LEN];
        out[0] = self.session;
        out[1..5].copy_from_slice(&self.len.to_le_bytes());
        out[5..37].copy_from_slice(&self.sha256);
        out[37] = self.version.major;
        out[38] = self.version.minor;
        out[39] = self.version.patch;
        out[40] = self.board.as_u8();
        out
    }
}

/// P4 → node: image bytes at an offset.
///
/// [`session`](Self::session), [`offset`](Self::offset), then 1 to
/// [`UPDATE_CHUNK`] bytes of data running to the end of the body. The data is borrowed from
/// the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateChunk<'a> {
    /// The session this chunk belongs to.
    pub session: u8,
    /// Where in the image `data` starts.
    pub offset: u32,
    /// Image bytes, 1 to [`UPDATE_CHUNK`] of them.
    pub data: &'a [u8],
}

impl<'a> UpdateChunk<'a> {
    /// Decode a body.
    ///
    /// # Errors
    /// [`DecodeError::TooShort`] for a body with no data, [`DecodeError::BadLength`] for
    /// more than [`UPDATE_CHUNK`] bytes of it, and see [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        if buf.len() <= UPDATE_CHUNK_HEADER {
            return Err(DecodeError::TooShort { need: UPDATE_CHUNK_HEADER + 1, got: buf.len() });
        }
        if buf.len() > UPDATE_CHUNK_MAX {
            return Err(DecodeError::BadLength { need: UPDATE_CHUNK_MAX, got: buf.len() });
        }
        Ok(Self { session: buf[0], offset: le_u32(buf, 1), data: &buf[UPDATE_CHUNK_HEADER..] })
    }

    /// Write the body into `out`, returning how many bytes it took.
    ///
    /// `None` if `data` is empty or longer than [`UPDATE_CHUNK`], or `out` is too small;
    /// [`UPDATE_CHUNK_MAX`] is always enough. Nothing is written when it fails.
    #[must_use]
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        if self.data.is_empty() || self.data.len() > UPDATE_CHUNK {
            return None;
        }
        let len = UPDATE_CHUNK_HEADER + self.data.len();
        let out = out.get_mut(..len)?;
        out[0] = self.session;
        out[1..UPDATE_CHUNK_HEADER].copy_from_slice(&self.offset.to_le_bytes());
        out[UPDATE_CHUNK_HEADER..].copy_from_slice(self.data);
        Some(len)
    }
}

/// P4 → node: every chunk is sent; check the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCommit {
    /// The session to check.
    pub session: u8,
}

impl UpdateCommit {
    /// Decode a body.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, UPDATE_COMMIT_LEN)?;
        Ok(Self { session: buf[0] })
    }

    /// Encode to the [`UPDATE_COMMIT_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; UPDATE_COMMIT_LEN] {
        let mut out = [0u8; UPDATE_COMMIT_LEN];
        out[0] = self.session;
        out
    }
}

/// P4 → node: drop the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateAbort {
    /// The session to drop. Any other session is left alone.
    pub session: u8,
}

impl UpdateAbort {
    /// Decode a body.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, UPDATE_ABORT_LEN)?;
        Ok(Self { session: buf[0] })
    }

    /// Encode to the [`UPDATE_ABORT_LEN`] bytes a node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; UPDATE_ABORT_LEN] {
        let mut out = [0u8; UPDATE_ABORT_LEN];
        out[0] = self.session;
        out
    }
}

/// P4 → node: confirm the image on trial, ending its trial.
///
/// An empty body. A node runs a fresh image on trial ([`CAP_FLAG_TRIAL`]) until it hears
/// the host; this is the message an update sends to say so. It names no session, because
/// the session ended with the reboot.
///
/// [`CAP_FLAG_TRIAL`]: super::CAP_FLAG_TRIAL
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateConfirm;

impl UpdateConfirm {
    /// Decode a body.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, UPDATE_CONFIRM_LEN)?;
        Ok(Self)
    }

    /// Encode to the [`UPDATE_CONFIRM_LEN`] bytes a node expects.
    #[must_use]
    pub const fn encode(&self) -> [u8; UPDATE_CONFIRM_LEN] {
        []
    }
}

/// Where a node's update session stands, as [`UpdateStatus`] carries it.
///
/// Each byte is fixed once assigned. A byte this build has no name for decodes to
/// [`UpdateState::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdateState {
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

impl UpdateState {
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

/// Node → P4: where an update session stands.
///
/// Six bytes: [`session`](Self::session), [`state`](Self::state) and
/// [`next_offset`](Self::next_offset). The host's flow control rides on `next_offset`
/// ([`crate::ota`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateStatus {
    /// The session the status answers.
    pub session: u8,
    /// Where it stands.
    pub state: UpdateState,
    /// The image offset the node wants next. 0 when there is no session.
    pub next_offset: u32,
}

impl UpdateStatus {
    /// Decode a body.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, UPDATE_STATUS_LEN)?;
        Ok(Self {
            session: buf[0],
            state: UpdateState::from_u8(buf[1]),
            next_offset: le_u32(buf, 2),
        })
    }

    /// Encode to the [`UPDATE_STATUS_LEN`] bytes a host expects.
    #[must_use]
    pub fn encode(&self) -> [u8; UPDATE_STATUS_LEN] {
        let mut out = [0u8; UPDATE_STATUS_LEN];
        out[0] = self.session;
        out[1] = self.state.as_u8();
        out[2..].copy_from_slice(&self.next_offset.to_le_bytes());
        out
    }
}
