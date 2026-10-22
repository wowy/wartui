//! The payload of a [`Kind::Update`](super::Kind::Update) transfer, and the P4's side of an
//! update.
//!
//! **Frozen with the rest of the core**, so any P4 build can update any C6 build that takes
//! updates. That is why it is a fixed layout written out by hand rather than postcard, as the
//! hello is. An op byte, then the body, little-endian:
//!
//! | Op | Direction | Body |
//! |---|---|---|
//! | 1 `Begin` | P4 to C6 | session (1), len (4), SHA-256 of the whole image (32) |
//! | 2 `Chunk` | P4 to C6 | session (1), offset (4), up to [`OTA_CHUNK`] image bytes to the payload's end |
//! | 3 `Commit` | P4 to C6 | session (1) |
//! | 4 `Install` | P4 to C6 | session (1) |
//! | 5 `Status` | C6 to P4 | session (1), state (1, [`OtaState`]'s code), next offset (4) |
//!
//! The C6 drives [`crate::ota::Receiver`] with these, as a node does with the air's update
//! frames, so the receiver's flow control applies: a status on each sector flush, and on each
//! duplicate or gap. A fixed op reads its body and ignores anything after it, so a body can
//! grow at its end.
//!
//! [`Sender`] is the P4's half, a pure state machine so the host tests it.
//!
//! - **Stop-and-wait per sector.** It sends a sector's chunks, then nothing until the status
//!   for that sector arrives. The C6's flash work holds its link loop, so a chunk sent during
//!   a flush waits behind it anyway, and the window keeps the C6 from falling behind.
//! - **The status's `next_offset` is where it resumes**, ahead or behind, so a chunk lost to
//!   a bad CRC costs one rewind. Statuses trail the ops by a transfer or two, so the chunks
//!   already in flight past a loss draw statuses naming the same offset. The sender ignores
//!   those after rewinding to it; resending for each would send every chunk after it twice.
//! - **Silence re-sends the last op that wanted an answer**, every [`REPLY_WAIT_MS`], and gives
//!   up after [`TRIES`]. A re-sent chunk always draws a status: taken, it flushes its sector;
//!   already held, it is a duplicate.
//! - **A C6 that reports no session is sent `Begin` again**, at most [`TRIES`] times.
//! - **`Install` goes every [`REPLY_WAIT_MS`] until the link stops asking**, which it does
//!   when the C6's next hello ends the update (`crate::split`). A lost `Install` would
//!   leave the C6 on its old image. A repeat is harmless: a C6 that installed has rebooted
//!   and holds no session, and one that did not answers with a status, which the sender
//!   ignores.

use crate::air::{OTA_CHUNK, OtaState, OtaStatusMsg};
use crate::ota::SECTOR;

/// How long the [`Sender`] waits for a status before sending again.
///
/// A sector flush on the C6 erases and writes 4 KB, tens of milliseconds, and the status
/// trails it by two transfers.
pub const REPLY_WAIT_MS: u64 = 500;

/// Sends of one op without an answer before the [`Sender`] gives up.
pub const TRIES: u32 = 5;

/// The longest encoded op: a full chunk.
pub const OP_MAX: usize = CHUNK_HEADER + OTA_CHUNK;

const _: () = assert!(OP_MAX <= super::PAYLOAD_MAX);

const BEGIN_LEN: usize = 1 + 1 + 4 + 32;
const CHUNK_HEADER: usize = 1 + 1 + 4;
const SESSION_LEN: usize = 1 + 1;
const STATUS_LEN: usize = 1 + 1 + 1 + 4;

/// One update op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op<'a> {
    /// Open a session for an image of `len` bytes whose SHA-256 is `sha256`.
    Begin {
        /// The session every later op names.
        session: u8,
        /// Image length in bytes.
        len: u32,
        /// SHA-256 of the whole image, as [`crate::ota::Receiver`] checks it.
        sha256: [u8; 32],
    },
    /// Image bytes at `offset`.
    Chunk {
        /// The session.
        session: u8,
        /// Where `data` starts in the image.
        offset: u32,
        /// At most [`OTA_CHUNK`] bytes.
        data: &'a [u8],
    },
    /// Check the image.
    Commit {
        /// The session.
        session: u8,
    },
    /// Boot the verified image.
    Install {
        /// The session.
        session: u8,
    },
    /// Where the C6's session stands.
    Status(OtaStatusMsg),
}

impl Op<'_> {
    /// Writes the op to the start of `out` and returns its length.
    ///
    /// # Panics
    /// If `out` is shorter than the op, or a chunk's data is longer than [`OTA_CHUNK`].
    pub fn encode(&self, out: &mut [u8]) -> usize {
        match *self {
            Op::Begin { session, len, sha256 } => {
                let out = &mut out[..BEGIN_LEN];
                out[0] = op_code(self);
                out[1] = session;
                out[2..6].copy_from_slice(&len.to_le_bytes());
                out[6..38].copy_from_slice(&sha256);
                BEGIN_LEN
            }
            Op::Chunk { session, offset, data } => {
                assert!(data.len() <= OTA_CHUNK, "chunk of {} bytes", data.len());
                let end = CHUNK_HEADER + data.len();
                let out = &mut out[..end];
                out[0] = op_code(self);
                out[1] = session;
                out[2..6].copy_from_slice(&offset.to_le_bytes());
                out[CHUNK_HEADER..].copy_from_slice(data);
                end
            }
            Op::Commit { session } | Op::Install { session } => {
                let out = &mut out[..SESSION_LEN];
                out[0] = op_code(self);
                out[1] = session;
                SESSION_LEN
            }
            Op::Status(status) => {
                let out = &mut out[..STATUS_LEN];
                out[0] = op_code(self);
                out[1] = status.session;
                out[2] = status.state.as_u8();
                out[3..7].copy_from_slice(&status.next_offset.to_le_bytes());
                STATUS_LEN
            }
        }
    }
}

impl<'a> Op<'a> {
    /// Reads an op from a transfer's payload.
    ///
    /// `None` for an op this build does not know, a payload shorter than its op, or a chunk
    /// longer than [`OTA_CHUNK`].
    #[must_use]
    pub fn decode(payload: &'a [u8]) -> Option<Self> {
        let (&op, body) = payload.split_first()?;
        match op {
            1 => {
                let b = body.get(..BEGIN_LEN - 1)?;
                Some(Op::Begin {
                    session: b[0],
                    len: le_u32(&b[1..5]),
                    sha256: *b[5..37].first_chunk()?,
                })
            }
            2 => {
                let b = body.get(..CHUNK_HEADER - 1)?;
                let data = &body[CHUNK_HEADER - 1..];
                if data.len() > OTA_CHUNK {
                    return None;
                }
                Some(Op::Chunk { session: b[0], offset: le_u32(&b[1..5]), data })
            }
            3 => Some(Op::Commit { session: *body.first()? }),
            4 => Some(Op::Install { session: *body.first()? }),
            5 => {
                let b = body.get(..STATUS_LEN - 1)?;
                Some(Op::Status(OtaStatusMsg {
                    session: b[0],
                    state: OtaState::from_u8(b[1]),
                    next_offset: le_u32(&b[2..6]),
                }))
            }
            _ => None,
        }
    }
}

const fn op_code(op: &Op<'_>) -> u8 {
    match op {
        Op::Begin { .. } => 1,
        Op::Chunk { .. } => 2,
        Op::Commit { .. } => 3,
        Op::Install { .. } => 4,
        Op::Status(_) => 5,
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Where a [`Sender`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Still sending, or waiting for a status.
    Sending,
    /// `Install` went out. Only `Install` is sent again.
    Installed,
    /// The C6 refused the image, or stopped answering.
    Failed(Failure),
}

/// Why a [`Sender`] gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The C6 reported this state: a hash mismatch, a flash error, another board or an image
    /// too large for its slot.
    Refused(OtaState),
    /// [`TRIES`] sends of one op went unanswered.
    Silent,
    /// The C6 reported holding no session more than [`TRIES`] times.
    LostSession,
}

/// The next op to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Begin,
    Chunks,
    Commit,
    Install,
}

/// An op sent that wants a status, to send again if none comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Begin,
    /// The chunk at this offset, the last of its sector.
    Chunk(u32),
    Commit,
}

/// Sends one image to the C6 over [`Op`]s; see the module documentation.
#[derive(Debug, Clone)]
pub struct Sender<'a> {
    image: &'a [u8],
    sha256: [u8; 32],
    session: u8,
    step: Step,
    /// The next chunk to send.
    offset: u32,
    /// The furthest `next_offset` the C6 has reported.
    acked: u32,
    /// The op awaiting a status, and when it was last sent.
    asked: Option<(Asked, u64)>,
    /// Sends of the op awaiting a status.
    tries: u32,
    /// Restarts after the C6 reported no session.
    restarts: u32,
    /// The offset of the last rewind, until a status reports the C6 past it.
    rewound_to: Option<u32>,
    /// When `Install` was last sent.
    install_sent_ms: u64,
    stage: Stage,
}

impl<'a> Sender<'a> {
    /// A sender for `image`, whose whole-file SHA-256 is `sha256` and whose appended digest
    /// is `digest`.
    ///
    /// The session byte comes from the digest, so two images never share one. It is never 0,
    /// the session a receiver with none reports.
    ///
    /// # Panics
    /// If `image` is longer than `u32::MAX` bytes.
    #[must_use]
    pub fn new(image: &'a [u8], sha256: [u8; 32], digest: &[u8; 32]) -> Self {
        assert!(u32::try_from(image.len()).is_ok(), "image past u32::MAX bytes");
        Self {
            image,
            sha256,
            session: digest[0].max(1),
            step: Step::Begin,
            offset: 0,
            acked: 0,
            asked: None,
            tries: 0,
            restarts: 0,
            rewound_to: None,
            install_sent_ms: 0,
            stage: Stage::Sending,
        }
    }

    /// Writes the next op to send into `out` and returns its length, or `None` when there is
    /// nothing to send at P4 time `now_ms`.
    ///
    /// # Panics
    /// If `out` is shorter than [`OP_MAX`].
    pub fn next(&mut self, now_ms: u64, out: &mut [u8]) -> Option<usize> {
        match self.stage {
            Stage::Sending => {}
            Stage::Installed => {
                if now_ms.saturating_sub(self.install_sent_ms) < REPLY_WAIT_MS {
                    return None;
                }
                self.install_sent_ms = now_ms;
                return Some(Op::Install { session: self.session }.encode(out));
            }
            Stage::Failed(_) => return None,
        }
        if let Some((asked, at)) = self.asked {
            if now_ms.saturating_sub(at) < REPLY_WAIT_MS {
                return None;
            }
            if self.tries >= TRIES {
                self.stage = Stage::Failed(Failure::Silent);
                return None;
            }
            self.tries += 1;
            self.asked = Some((asked, now_ms));
            // The silence outlasted anything in flight, so this op's status is current, even
            // one naming the last rewind's offset again.
            self.rewound_to = None;
            return Some(self.encode_asked(asked, out));
        }
        let len = self.len();
        match self.step {
            Step::Begin => Some(self.ask(Asked::Begin, now_ms, out)),
            Step::Chunks => {
                let start = self.offset;
                let end = start.saturating_add(OTA_CHUNK as u32).min(len);
                self.offset = end;
                if (end as usize).is_multiple_of(SECTOR) {
                    // The receiver flushes the sector and reports.
                    return Some(self.ask(Asked::Chunk(start), now_ms, out));
                }
                if end == len {
                    // A partial last sector flushes only on commit.
                    self.step = Step::Commit;
                }
                Some(self.chunk(start, out))
            }
            Step::Commit => Some(self.ask(Asked::Commit, now_ms, out)),
            Step::Install => {
                self.stage = Stage::Installed;
                self.install_sent_ms = now_ms;
                Some(Op::Install { session: self.session }.encode(out))
            }
        }
    }

    /// Take a status from the C6. One naming another session is ignored.
    pub fn on_status(&mut self, status: &OtaStatusMsg) {
        if status.session != self.session || self.stage != Stage::Sending {
            return;
        }
        let len = self.len();
        match status.state {
            OtaState::Receiving => {
                let next = status.next_offset.min(len);
                // A chunk sent before the rewind to `next`, reporting the gap it fell in.
                if self.rewound_to == Some(next) {
                    return;
                }
                self.rewound_to = (next < self.offset).then_some(next);
                self.asked = None;
                self.tries = 0;
                self.acked = self.acked.max(next);
                if next >= len {
                    self.step = Step::Commit;
                } else {
                    self.step = Step::Chunks;
                    self.offset = next;
                }
            }
            OtaState::Verified => {
                self.asked = None;
                self.acked = len;
                self.step = Step::Install;
            }
            // The C6 holds no session: it lost it, so start again, at most `TRIES` times. Until
            // `Begin` is answered, an op sent before it may still report the lost session.
            OtaState::Idle if self.step == Step::Begin => {}
            OtaState::Idle => {
                self.asked = None;
                self.rewound_to = None;
                self.restarts += 1;
                if self.restarts > TRIES {
                    self.stage = Stage::Failed(Failure::LostSession);
                }
                self.step = Step::Begin;
                self.offset = 0;
            }
            state @ (OtaState::HashMismatch
            | OtaState::WrongBoard
            | OtaState::TooLarge
            | OtaState::FlashError
            | OtaState::Unknown(_)) => self.stage = Stage::Failed(Failure::Refused(state)),
        }
    }

    /// Where the sender stands.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The share of the image the C6 has reported holding, 0 to 100.
    #[must_use]
    pub fn percent(&self) -> u8 {
        let len = u64::from(self.len().max(1));
        // At most 100.
        #[allow(clippy::cast_possible_truncation)]
        let percent = (u64::from(self.acked) * 100 / len) as u8;
        percent
    }

    fn len(&self) -> u32 {
        // `new` checked it fits.
        #[allow(clippy::cast_possible_truncation)]
        let len = self.image.len() as u32;
        len
    }

    fn ask(&mut self, asked: Asked, now_ms: u64, out: &mut [u8]) -> usize {
        self.asked = Some((asked, now_ms));
        self.tries = 1;
        self.encode_asked(asked, out)
    }

    fn encode_asked(&self, asked: Asked, out: &mut [u8]) -> usize {
        match asked {
            Asked::Begin => {
                Op::Begin { session: self.session, len: self.len(), sha256: self.sha256 }
                    .encode(out)
            }
            Asked::Chunk(start) => self.chunk(start, out),
            Asked::Commit => Op::Commit { session: self.session }.encode(out),
        }
    }

    fn chunk(&self, start: u32, out: &mut [u8]) -> usize {
        let end = (start as usize + OTA_CHUNK).min(self.image.len());
        let data = &self.image[start as usize..end];
        Op::Chunk { session: self.session, offset: start, data }.encode(out)
    }
}
