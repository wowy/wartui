//! The node's half of an over-the-air update: a state machine that takes the update
//! frames ([`crate::air::OtaBeginMsg`] and the rest) and writes the image into the spare
//! app slot through a [`SectorSink`].
//!
//! It decides nothing about versions and touches neither `otadata` nor the reset. A
//! downgrade is the host's call, and switching slots is the firmware's once
//! [`Receiver::is_verified`] says the image is whole.
//!
//! **Flow control rides on `next_offset`, not on MAC acks.** An ack proves the frame reached
//! the radio, not that the node read it. The node's receive queue is 10 deep and a flash
//! write holds the loop that drains it, so acked chunks are dropped while a sector is
//! written. Every [`OtaStatusMsg`] names the next offset the node wants. The node reports
//! one on each sector flush, every four chunks, which gives the host a window to pace
//! itself by. It reports one on each duplicate or gap too, so the host goes back to the
//! first byte the node lacks rather than guessing which chunks fell out of the queue.
//!
//! **Erase is lazy**, one sector immediately before that sector's write. Erasing the whole
//! slot at `begin` would stall the node for seconds while the host waits on a status, and
//! erases sectors a shorter image never reaches.
//!
//! **A `begin` identical to a session receiving or verified changes nothing.** The host
//! re-sends [`OtaBeginMsg`] when the status answering the first is lost, and restarting
//! would throw away chunks the node already holds. After a hash mismatch or a flash error
//! there is nothing to keep, so an identical `begin` starts afresh. A host that restarts
//! and reuses its session byte is then never stuck behind a failure.
//!
//! **An image fills whole sectors only.** [`Receiver::begin`] measures `len` against the
//! slot's capacity rounded down to a [`SECTOR`], so the last erase never reaches past the
//! slot.
//!
//! **The hash runs as bytes arrive**, in order, so `commit` costs one comparison. Reading a
//! 1.9 MB slot back to hash it would cost seconds, and a chunk is only accepted at
//! `next_offset`, so the running hash covers exactly the bytes written.

pub mod trial;

use sha2::{Digest, Sha256};

use crate::air::{
    Board, OTA_CHUNK, OtaAbortMsg, OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg,
    Version,
};

/// Bytes in one flash sector: the unit erased, and the unit [`Receiver`] buffers.
pub const SECTOR: usize = 4096;

const _: () = assert!(SECTOR.is_multiple_of(OTA_CHUNK));

/// The flash slot an image is written into.
pub trait SectorSink {
    /// Why an erase or write failed.
    type Error;
    /// Bytes the target slot holds.
    fn capacity(&self) -> u32;
    /// Erase the [`SECTOR`]-byte sector starting at `offset`, a multiple of [`SECTOR`].
    ///
    /// # Errors
    /// Whatever the flash reports.
    fn erase_sector(&mut self, offset: u32) -> Result<(), Self::Error>;
    /// Write `data` at `offset`. `offset` and `data.len()` are multiples of 4.
    ///
    /// # Errors
    /// Whatever the flash reports.
    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error>;
}

/// Where the current session stands, past the ones `begin` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Receiving,
    Verified,
    HashMismatch,
    FlashError,
}

impl Phase {
    const fn state(self) -> OtaState {
        match self {
            Self::Receiving => OtaState::Receiving,
            Self::Verified => OtaState::Verified,
            Self::HashMismatch => OtaState::HashMismatch,
            Self::FlashError => OtaState::FlashError,
        }
    }
}

#[derive(Debug, Clone)]
struct Session {
    begin: OtaBeginMsg,
    next_offset: u32,
    phase: Phase,
    hasher: Sha256,
}

/// Receives one image at a time into a [`SectorSink`].
///
/// Holds a [`SECTOR`] of buffer inline; [`Receiver::new`] is `const` so it can live in a
/// `static`.
#[derive(Debug, Clone)]
pub struct Receiver {
    board: Board,
    session: Option<Session>,
    buf: [u8; SECTOR],
}

impl Receiver {
    /// A receiver for images built for `board`, with no session.
    #[must_use]
    pub const fn new(board: Board) -> Self {
        Self { board, session: None, buf: [0; SECTOR] }
    }

    /// Open a session, or answer for the one already open.
    ///
    /// Refused with [`OtaState::WrongBoard`] or [`OtaState::TooLarge`] (an empty image too,
    /// or one past the slot's last whole sector), leaving any current session as it is. A
    /// `begin` identical to a session receiving or verified returns its status and keeps its
    /// progress. Any other starts afresh.
    pub fn begin<S: SectorSink>(&mut self, msg: &OtaBeginMsg, sink: &S) -> OtaStatusMsg {
        if msg.board != self.board {
            return refused(msg.session, OtaState::WrongBoard);
        }
        let whole_sectors = sink.capacity() / SECTOR as u32 * SECTOR as u32;
        if msg.len == 0 || msg.len > whole_sectors {
            return refused(msg.session, OtaState::TooLarge);
        }
        if let Some(session) = &self.session
            && session.begin == *msg
            && matches!(session.phase, Phase::Receiving | Phase::Verified)
        {
            return self.status();
        }
        self.session = Some(Session {
            begin: *msg,
            next_offset: 0,
            phase: Phase::Receiving,
            hasher: Sha256::new(),
        });
        self.status()
    }

    /// Take a chunk.
    ///
    /// Accepted only at `next_offset`, and only as a full [`OTA_CHUNK`] or as the image's
    /// last bytes. Reports a status when a sector is written, and when the chunk is not
    /// accepted: a duplicate, a gap, a chunk of the wrong length, or one for a session this
    /// receiver does not hold ([`OtaState::Idle`]).
    pub fn chunk<S: SectorSink>(
        &mut self,
        msg: &OtaChunkMsg<'_>,
        sink: &mut S,
    ) -> Option<OtaStatusMsg> {
        let Some(session) = self.session.as_mut().filter(|s| s.begin.session == msg.session) else {
            return Some(idle(msg.session));
        };
        if session.phase != Phase::Receiving || msg.offset != session.next_offset {
            return Some(self.status());
        }
        let remaining = session.begin.len - session.next_offset;
        let want = OTA_CHUNK.min(remaining as usize);
        if msg.data.len() != want {
            return Some(self.status());
        }
        let at = session.next_offset as usize % SECTOR;
        self.buf[at..at + want].copy_from_slice(msg.data);
        session.hasher.update(msg.data);
        // `want` is at most `OTA_CHUNK`.
        #[allow(clippy::cast_possible_truncation)]
        {
            session.next_offset += want as u32;
        }
        if !(session.next_offset as usize).is_multiple_of(SECTOR) {
            return None;
        }
        let start = session.next_offset - SECTOR as u32;
        if flush(sink, start, &mut self.buf, SECTOR).is_err() {
            session.phase = Phase::FlashError;
        }
        Some(self.status())
    }

    /// Check the image.
    ///
    /// With bytes still missing, reports [`OtaState::Receiving`] and the first of them.
    /// Otherwise writes the last partial sector and reports [`OtaState::Verified`] or
    /// [`OtaState::HashMismatch`]. Once the session is past receiving, reports where it
    /// ended.
    pub fn commit<S: SectorSink>(&mut self, msg: &OtaCommitMsg, sink: &mut S) -> OtaStatusMsg {
        let Some(session) = self.session.as_mut().filter(|s| s.begin.session == msg.session) else {
            return idle(msg.session);
        };
        if session.phase != Phase::Receiving || session.next_offset < session.begin.len {
            return self.status();
        }
        let fill = session.next_offset as usize % SECTOR;
        if fill != 0 {
            let start = session.next_offset - fill as u32;
            if flush(sink, start, &mut self.buf, fill).is_err() {
                session.phase = Phase::FlashError;
                return self.status();
            }
        }
        let digest = session.hasher.finalize_reset();
        session.phase =
            if digest[..] == session.begin.sha256 { Phase::Verified } else { Phase::HashMismatch };
        self.status()
    }

    /// Drop the session if `msg` names it, and report [`OtaState::Idle`].
    pub fn abort(&mut self, msg: &OtaAbortMsg) -> OtaStatusMsg {
        if self.session.as_ref().is_some_and(|s| s.begin.session == msg.session) {
            self.session = None;
        }
        idle(msg.session)
    }

    /// Drop any session, as the firmware does when the host goes quiet.
    pub fn reset(&mut self) {
        self.session = None;
    }

    /// Where the current session stands, or [`OtaState::Idle`] for session 0 with none.
    #[must_use]
    pub fn status(&self) -> OtaStatusMsg {
        match &self.session {
            Some(session) => OtaStatusMsg {
                session: session.begin.session,
                state: session.phase.state(),
                next_offset: session.next_offset,
            },
            None => idle(0),
        }
    }

    /// The board this receiver takes images for.
    #[must_use]
    pub const fn board(&self) -> Board {
        self.board
    }

    /// Whether a session is open, in any state.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// Whether the current session's image is whole and matched its hash.
    #[must_use]
    pub fn is_verified(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.phase == Phase::Verified)
    }

    /// The current session's image length.
    #[must_use]
    pub fn total_len(&self) -> Option<u32> {
        self.session.as_ref().map(|s| s.begin.len)
    }

    /// The firmware version the current session's image carries.
    #[must_use]
    pub fn version(&self) -> Option<Version> {
        self.session.as_ref().map(|s| s.begin.version)
    }
}

const fn idle(session: u8) -> OtaStatusMsg {
    OtaStatusMsg { session, state: OtaState::Idle, next_offset: 0 }
}

const fn refused(session: u8, state: OtaState) -> OtaStatusMsg {
    OtaStatusMsg { session, state, next_offset: 0 }
}

/// Erase the sector at `start` and write the first `fill` bytes of `buf` into it, padded
/// with 0xFF to a multiple of 4. The padding is what erased flash holds anyway.
fn flush<S: SectorSink>(
    sink: &mut S,
    start: u32,
    buf: &mut [u8; SECTOR],
    fill: usize,
) -> Result<(), S::Error> {
    let padded = fill.next_multiple_of(4);
    buf[fill..padded].fill(0xFF);
    sink.erase_sector(start)?;
    sink.write(start, &buf[..padded])
}
