//! The two halves of stop-and-wait on the node bus: what a receiver acknowledges, and what a
//! sender waits for.

use super::Header;

/// How long a sender waits for its records to be acknowledged before sending them again.
///
/// A node stamps its ack when its main loop has checked the transfer, which can be a dwell
/// later, so this is several dwells.
pub const ACK_TIMEOUT_MS: u64 = 1_000;

/// Transfers one set of records goes out in, the first included, before the sender gives up.
pub const SEND_TRIES: u8 = 3;

/// What a receiver acknowledges: the last good transfer from its peer that carried records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckTracker {
    last: Option<u16>,
}

impl AckTracker {
    /// Nothing taken yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// The records in transfer `seq` were taken. An idle transfer is never noted.
    pub fn note_records(&mut self, seq: u16) {
        self.last = Some(seq);
    }

    /// The ack to stamp, if any.
    #[must_use]
    pub const fn ack(&self) -> Option<u16> {
        self.last
    }
}

/// What [`AckWait::poll`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckPoll {
    /// Nothing is waiting.
    Idle,
    /// Records are waiting, inside [`ACK_TIMEOUT_MS`].
    Waiting,
    /// The timeout passed with tries left: send the same records again.
    Resend,
    /// The last try timed out. The wait is over.
    GaveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Waiting {
    seq: u16,
    sent_ms: u64,
    tries: u8,
}

/// A sender's wait for one transfer's records to be acknowledged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckWait {
    waiting: Option<Waiting>,
}

impl AckWait {
    /// Nothing waiting.
    #[must_use]
    pub const fn new() -> Self {
        Self { waiting: None }
    }

    /// The records went out in transfer `seq` at `now_ms`: the first try, or a resend of the
    /// records already waited on.
    pub fn sent(&mut self, seq: u16, now_ms: u64) {
        let tries = self.waiting.map_or(1, |w| w.tries.saturating_add(1));
        self.waiting = Some(Waiting { seq, sent_ms: now_ms, tries });
    }

    /// Whether records are waiting for their ack.
    #[must_use]
    pub const fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// Read a header from the peer. Returns `true`, and ends the wait, when it acknowledges the
    /// transfer waited on.
    pub fn on_header(&mut self, header: &Header) -> bool {
        match (self.waiting, header.acked()) {
            (Some(waiting), Some(ack)) if ack == waiting.seq => {
                self.waiting = None;
                true
            }
            _ => false,
        }
    }

    /// Whether a resend is due at `now_ms`, or the wait is over. [`AckPoll::GaveUp`] ends it.
    pub fn poll(&mut self, now_ms: u64) -> AckPoll {
        let Some(waiting) = self.waiting else { return AckPoll::Idle };
        if now_ms.saturating_sub(waiting.sent_ms) < ACK_TIMEOUT_MS {
            return AckPoll::Waiting;
        }
        if waiting.tries < SEND_TRIES {
            return AckPoll::Resend;
        }
        self.waiting = None;
        AckPoll::GaveUp
    }

    /// When [`AckWait::poll`] next changes its answer, if anything waits.
    #[must_use]
    pub fn deadline_ms(&self) -> Option<u64> {
        self.waiting.map(|w| w.sent_ms + ACK_TIMEOUT_MS)
    }

    /// Drop the wait without an answer.
    pub fn cancel(&mut self) {
        self.waiting = None;
    }
}
