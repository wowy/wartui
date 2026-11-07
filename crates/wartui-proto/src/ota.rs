//! The node's half of an update: a state machine that takes the update messages
//! ([`crate::bus::UpdateBegin`] and the rest) and writes the image into the spare app slot
//! through a [`SectorSink`].
//!
//! It decides nothing about versions and touches neither `otadata` nor the reset. A
//! downgrade is the host's call, and switching slots is the firmware's once
//! [`Receiver::is_verified`] says the image is whole.
//!
//! **Flow control rides on `next_offset`, not on the transport's acks.** An ack proves a
//! chunk reached the node, not that the receiver took it: a resent chunk arrives twice, and a
//! chunk after a lost one arrives out of order. Every [`UpdateStatus`] names the next offset
//! the node wants. The node reports one on each sector flush, every four chunks, which gives
//! the host a window to pace itself by. It reports one on each duplicate or gap too, so the
//! host goes back to the first byte the node lacks rather than guessing.
//!
//! **Erase is lazy**, immediately before the first write it clears. Erasing the whole slot at
//! `begin` would stall the node for seconds while the host waits on a status, and erases
//! sectors a shorter image never reaches. A sink that can erases a [`BLOCK`] when the image
//! reaches one, since a node takes nothing over its bus while flash is erased: on the
//! carrier's nodes one block erase costs about 150–170 ms (inferred from update timings),
//! where sixteen sector flushes, each erasing its sector, cost about 43 ms apiece. Otherwise,
//! and where less than a block of the slot remains, it erases sector by sector. A block may
//! run past the image's end, never past the slot.
//!
//! **A `begin` identical to a session receiving or verified changes nothing.** The host
//! re-sends [`UpdateBegin`] when the status answering the first is lost, and restarting
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
//!
//! The P4 bridge's C6 takes images through the same receiver, over the UART link
//! ([`crate::radio_link::update`]), and so does the P4 itself, over its host link
//! ([`crate::link::HostToBridge::Update`]). Both boot them on the same trial ([`TRIAL_BOOTS`]).
//! [`image`] reads the digest that names an image.

pub mod image;
pub mod trial;

use sha2::{Digest, Sha256};

use crate::bus::{
    Board, UPDATE_CHUNK, UpdateAbort, UpdateBegin, UpdateChunk, UpdateCommit, UpdateState,
    UpdateStatus, Version,
};

/// Bytes in one flash sector: the unit erased, and the unit [`Receiver`] buffers.
pub const SECTOR: usize = 4096;

/// Bytes in one flash block: the unit [`SectorSink::erase_block`] erases, when it can.
pub const BLOCK: usize = 65536;

/// Unconfirmed boots of a new image before it is rolled back, on a node or either half of a P4
/// bridge.
///
/// Three rather than one: a node runs on a USB-C PD supply, which can reset it once while it
/// negotiates, so a single boot that never heard the host is not evidence of a bad image. A
/// hung image reaches three too, since each reset counts: the operator's power cycle on a
/// node, the P4's reset of its C6 (`crate::split`), and the P4's own trial watchdog.
pub const TRIAL_BOOTS: u32 = 3;

const _: () = assert!(SECTOR.is_multiple_of(UPDATE_CHUNK));
const _: () = assert!(BLOCK.is_multiple_of(SECTOR));

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
    /// Erase the [`BLOCK`]-byte block starting at `offset`, a multiple of [`BLOCK`], or `None`
    /// when this sink cannot, and the receiver erases sector by sector.
    fn erase_block(&mut self, _offset: u32) -> Option<Result<(), Self::Error>> {
        None
    }
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
    const fn state(self) -> UpdateState {
        match self {
            Self::Receiving => UpdateState::Receiving,
            Self::Verified => UpdateState::Verified,
            Self::HashMismatch => UpdateState::HashMismatch,
            Self::FlashError => UpdateState::FlashError,
        }
    }
}

#[derive(Debug, Clone)]
struct Session {
    begin: UpdateBegin,
    next_offset: u32,
    /// Bytes from the slot's start a block erase has already cleared.
    erased_to: u32,
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
    /// Refused with [`UpdateState::WrongBoard`] or [`UpdateState::TooLarge`] (an empty image too,
    /// or one past the slot's last whole sector), leaving any current session as it is. A
    /// `begin` identical to a session receiving or verified returns its status and keeps its
    /// progress. Any other starts afresh.
    pub fn begin<S: SectorSink>(&mut self, msg: &UpdateBegin, sink: &S) -> UpdateStatus {
        if msg.board != self.board {
            return refused(msg.session, UpdateState::WrongBoard);
        }
        let whole_sectors = sink.capacity() / SECTOR as u32 * SECTOR as u32;
        if msg.len == 0 || msg.len > whole_sectors {
            return refused(msg.session, UpdateState::TooLarge);
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
            erased_to: 0,
            phase: Phase::Receiving,
            hasher: Sha256::new(),
        });
        self.status()
    }

    /// Take a chunk.
    ///
    /// Accepted only at `next_offset`, and only as a full [`UPDATE_CHUNK`] or as the image's
    /// last bytes. Reports a status when a sector is written, and when the chunk is not
    /// accepted: a duplicate, a gap, a chunk of the wrong length, or one for a session this
    /// receiver does not hold ([`UpdateState::Idle`]).
    pub fn chunk<S: SectorSink>(
        &mut self,
        msg: &UpdateChunk<'_>,
        sink: &mut S,
    ) -> Option<UpdateStatus> {
        let Some(session) = self.session.as_mut().filter(|s| s.begin.session == msg.session) else {
            return Some(idle(msg.session));
        };
        if session.phase != Phase::Receiving || msg.offset != session.next_offset {
            return Some(self.status());
        }
        let remaining = session.begin.len - session.next_offset;
        let want = UPDATE_CHUNK.min(remaining as usize);
        if msg.data.len() != want {
            return Some(self.status());
        }
        let at = session.next_offset as usize % SECTOR;
        self.buf[at..at + want].copy_from_slice(msg.data);
        session.hasher.update(msg.data);
        // `want` is at most `UPDATE_CHUNK`.
        #[allow(clippy::cast_possible_truncation)]
        {
            session.next_offset += want as u32;
        }
        if !(session.next_offset as usize).is_multiple_of(SECTOR) {
            return None;
        }
        let start = session.next_offset - SECTOR as u32;
        if flush(sink, start, &mut session.erased_to, &mut self.buf, SECTOR).is_err() {
            session.phase = Phase::FlashError;
        }
        Some(self.status())
    }

    /// Check the image.
    ///
    /// With bytes still missing, reports [`UpdateState::Receiving`] and the first of them.
    /// Otherwise writes the last partial sector and reports [`UpdateState::Verified`] or
    /// [`UpdateState::HashMismatch`]. Once the session is past receiving, reports where it
    /// ended.
    pub fn commit<S: SectorSink>(&mut self, msg: &UpdateCommit, sink: &mut S) -> UpdateStatus {
        let Some(session) = self.session.as_mut().filter(|s| s.begin.session == msg.session) else {
            return idle(msg.session);
        };
        if session.phase != Phase::Receiving || session.next_offset < session.begin.len {
            return self.status();
        }
        let fill = session.next_offset as usize % SECTOR;
        if fill != 0 {
            let start = session.next_offset - fill as u32;
            if flush(sink, start, &mut session.erased_to, &mut self.buf, fill).is_err() {
                session.phase = Phase::FlashError;
                return self.status();
            }
        }
        let digest = session.hasher.finalize_reset();
        session.phase =
            if digest[..] == session.begin.sha256 { Phase::Verified } else { Phase::HashMismatch };
        self.status()
    }

    /// Drop the session if `msg` names it, and report [`UpdateState::Idle`].
    pub fn abort(&mut self, msg: &UpdateAbort) -> UpdateStatus {
        if self.session.as_ref().is_some_and(|s| s.begin.session == msg.session) {
            self.session = None;
        }
        idle(msg.session)
    }

    /// Drop any session, as the firmware does when the host goes quiet.
    pub fn reset(&mut self) {
        self.session = None;
    }

    /// Where the current session stands, or [`UpdateState::Idle`] for session 0 with none.
    #[must_use]
    pub fn status(&self) -> UpdateStatus {
        match &self.session {
            Some(session) => UpdateStatus {
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

const fn idle(session: u8) -> UpdateStatus {
    UpdateStatus { session, state: UpdateState::Idle, next_offset: 0 }
}

const fn refused(session: u8, state: UpdateState) -> UpdateStatus {
    UpdateStatus { session, state, next_offset: 0 }
}

/// Write the first `fill` bytes of `buf` into the sector at `start`, padded with 0xFF to a
/// multiple of 4, erasing it first unless it lies below `erased_to`. The padding is what erased
/// flash holds anyway.
///
/// At a block boundary with a whole block left in the slot, a sink that can erases the block,
/// and `erased_to` moves past it.
fn flush<S: SectorSink>(
    sink: &mut S,
    start: u32,
    erased_to: &mut u32,
    buf: &mut [u8; SECTOR],
    fill: usize,
) -> Result<(), S::Error> {
    let padded = fill.next_multiple_of(4);
    buf[fill..padded].fill(0xFF);
    if start >= *erased_to {
        let whole_sectors = sink.capacity() / SECTOR as u32 * SECTOR as u32;
        let block = (start as usize).is_multiple_of(BLOCK)
            && start.checked_add(BLOCK as u32).is_some_and(|end| end <= whole_sectors);
        match block.then(|| sink.erase_block(start)).flatten() {
            Some(result) => {
                result?;
                *erased_to = start + BLOCK as u32;
            }
            None => sink.erase_sector(start)?,
        }
    }
    sink.write(start, &buf[..padded])
}
