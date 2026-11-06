//! The P4's queue of records for one slot, and the stop-and-wait that delivers them.
//!
//! The host names each record by an id. A record that cannot be queued is refused at once, so
//! the host hears why; one queued is answered once its transfer is acknowledged, or once the
//! sender gives up on it. Several queued records share a transfer while they fit, and wait for
//! one ack together: a *flight*. A flight is kept whole until its ack, and resent whole.

use heapless::Vec;

use super::{AckPoll, AckWait, HEADER_LEN, Header, PAYLOAD_MAX, Packer, RECORD_MAX, TRANSFER_LEN};
use crate::frame_ring::FrameRing;
use crate::link::Undelivered;

/// The most records one flight carries.
pub const FLIGHT_MAX: usize = 16;

/// The ids of a flight's records, in the order they were queued.
pub type Ids = Vec<u16, FLIGHT_MAX>;

/// Bytes ahead of a queued record's body: its id, then its type.
const ENTRY_HEADER: usize = 3;

/// One flight: its payload as it went out, and the ids in it.
struct Flight {
    payload: [u8; PAYLOAD_MAX],
    len: usize,
    ids: Ids,
    /// Due to go out: packed and not yet sent, or a resend is due.
    due: bool,
}

/// One slot's records, `B` bytes of them queued behind the flight.
pub struct Downlink<const B: usize> {
    queue: FrameRing<B>,
    flight: Flight,
    wait: AckWait,
    /// Records refused because the queue was full.
    refused: u32,
}

impl<const B: usize> Default for Downlink<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const B: usize> Downlink<B> {
    /// Nothing queued. Const, so firmware builds it in a static.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: FrameRing::new(),
            flight: Flight { payload: [0; PAYLOAD_MAX], len: 0, ids: Vec::new(), due: false },
            wait: AckWait::new(),
            refused: 0,
        }
    }

    /// Queue record `id` of type `kind`.
    ///
    /// # Errors
    /// [`Undelivered::TooLarge`] for a body no transfer carries, and
    /// [`Undelivered::QueueFull`] when the queue has no room. Nothing is queued then.
    pub fn push(&mut self, id: u16, kind: u8, body: &[u8]) -> Result<(), Undelivered> {
        if body.len() > RECORD_MAX {
            return Err(Undelivered::TooLarge);
        }
        let mut entry = [0u8; ENTRY_HEADER + RECORD_MAX];
        let len = ENTRY_HEADER + body.len();
        entry[..2].copy_from_slice(&id.to_le_bytes());
        entry[2] = kind;
        entry[ENTRY_HEADER..len].copy_from_slice(body);
        // Each entry sits behind the ring's two-byte length. Refused rather than evicting, so
        // no queued record goes unanswered.
        if FrameRing::<B>::CAPACITY - self.queue.len_bytes() < 2 + len
            || !self.queue.push(&entry[..len])
        {
            self.refused = self.refused.wrapping_add(1);
            return Err(Undelivered::QueueFull);
        }
        Ok(())
    }

    /// Whether the next transfer to this slot should carry records: a flight is due, or none is
    /// waits for its ack and records are queued.
    #[must_use]
    pub fn has_work(&self) -> bool {
        if self.wait.is_waiting() { self.flight.due } else { !self.queue.is_empty() }
    }

    /// Whether a flight waits for its ack.
    #[must_use]
    pub const fn is_waiting(&self) -> bool {
        self.wait.is_waiting()
    }

    /// When [`Downlink::poll`] next has something to say.
    #[must_use]
    pub fn deadline_ms(&self) -> Option<u64> {
        self.wait.deadline_ms()
    }

    /// Write what the next transfer carries at [`HEADER_LEN`] of `buf` and return its length:
    /// the flight when one is due, a new flight from the queue when none waits, or 0.
    pub fn fill(&mut self, buf: &mut [u8; TRANSFER_LEN]) -> usize {
        if !self.wait.is_waiting() && !self.flight.due {
            self.pack();
        }
        if !self.flight.due {
            return 0;
        }
        let len = self.flight.len;
        buf[HEADER_LEN..HEADER_LEN + len].copy_from_slice(&self.flight.payload[..len]);
        len
    }

    /// Pack a flight from the queue, as many records as fit.
    fn pack(&mut self) {
        let mut scratch = [0u8; TRANSFER_LEN];
        let mut packer = Packer::new(&mut scratch);
        let mut ids = Ids::new();
        let mut entry = [0u8; ENTRY_HEADER + RECORD_MAX];
        while ids.len() < FLIGHT_MAX && !self.queue.is_empty() {
            let Some(next) = self.queue.peek_len() else { break };
            if !packer.fits(next - ENTRY_HEADER) {
                break;
            }
            let (len, _) = self.queue.read(&mut entry);
            let id = u16::from_le_bytes([entry[0], entry[1]]);
            // Fits: checked above against the entry's length.
            packer.push(entry[2], &entry[ENTRY_HEADER..len]);
            let _ = ids.push(id);
        }
        let len = packer.len();
        self.flight.payload[..len].copy_from_slice(&scratch[HEADER_LEN..HEADER_LEN + len]);
        self.flight.len = len;
        self.flight.due = !ids.is_empty();
        self.flight.ids = ids;
    }

    /// The transfer [`Downlink::fill`] filled went out as `seq` at `now_ms`. Nothing when no
    /// flight is due, as once [`Downlink::on_header`] has delivered it.
    pub fn sent(&mut self, seq: u16, now_ms: u64) {
        if self.flight.due {
            self.flight.due = false;
            self.wait.sent(seq, now_ms);
        }
    }

    /// Read a header from the node that arrived in the P4's transfer `carried_in`: the flight's
    /// ids once it acknowledges them ([`AckWait::on_header`]).
    pub fn on_header(&mut self, header: &Header, carried_in: u16) -> Option<Ids> {
        if self.wait.on_header(header, carried_in) { Some(self.take_flight()) } else { None }
    }

    /// Mark a resend due when the timeout passes, and return the flight's ids once the last try
    /// has timed out.
    pub fn poll(&mut self, now_ms: u64) -> Option<Ids> {
        match self.wait.poll(now_ms) {
            AckPoll::Idle | AckPoll::Waiting => None,
            AckPoll::Resend => {
                self.flight.due = true;
                None
            }
            AckPoll::GaveUp => Some(self.take_flight()),
        }
    }

    /// Empty the flight and the queue, as for a slot whose node has gone, giving `each` every id.
    pub fn drain(&mut self, mut each: impl FnMut(u16)) {
        self.wait.cancel();
        for id in self.take_flight() {
            each(id);
        }
        let mut entry = [0u8; ENTRY_HEADER + RECORD_MAX];
        while !self.queue.is_empty() {
            self.queue.read(&mut entry);
            each(u16::from_le_bytes([entry[0], entry[1]]));
        }
    }

    fn take_flight(&mut self) -> Ids {
        self.flight.len = 0;
        self.flight.due = false;
        core::mem::take(&mut self.flight.ids)
    }

    /// The queue's size in bytes.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        B
    }

    /// The most bytes the queue has held.
    #[must_use]
    pub const fn high_water(&self) -> usize {
        self.queue.high_water()
    }

    /// Records refused because the queue was full, since boot.
    #[must_use]
    pub const fn refused(&self) -> u32 {
        self.refused
    }
}
