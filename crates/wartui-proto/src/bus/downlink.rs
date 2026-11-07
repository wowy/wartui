//! The P4's queue of records for one slot, and the window that delivers them.
//!
//! The host names each record by an id. A record that cannot be queued is refused at once, so
//! the host hears why; one queued is answered once its transfer is acknowledged, or once the
//! sender gives up on it. Several queued records share a transfer while they fit, and wait for
//! one ack together: a *flight*. A flight is kept whole until its ack, and resent whole.
//!
//! Up to [`WINDOW`] flights are out at once, numbered, and the node takes them strictly in order
//! ([`FlightTaker`](super::FlightTaker)). A new flight goes in the next transfer while there is
//! room, the one that fetches the last flight's ack included. An ack naming a flight's transfer
//! delivers it and every flight before it. A good header from the node that does not acknowledge
//! the oldest flight out means the node did not take it, since the node stamps every transfer it
//! arms with the ack of everything it took before. So the oldest flight and every one after it
//! are due again, at once, in order.
//!
//! [`ACK_TIMEOUT_MS`] without word from the node is the backstop. Each timeout of the oldest flight
//! sends the window again, and the [`SEND_TRIES`]th gives up on the whole window, answering every
//! flight in it, so no later flight is taken ahead of an earlier one the node never took. A
//! go-back does not restart the timer, so a node that drops the oldest flight on every try is
//! given up on as surely as one that never answers.

use heapless::Vec;

use super::window::{FlightTag, WINDOW, forward};
use super::{
    ACK_TIMEOUT_MS, HEADER_LEN, Header, PAYLOAD_MAX, Packer, RECORD_MAX, SEND_TRIES, TRANSFER_LEN,
    seal,
};
use crate::frame_ring::FrameRing;
use crate::link::{MAX_NODE_DOWN, Undelivered};

/// The most records one flight carries.
pub const FLIGHT_MAX: usize = 16;

/// The ids of the records one call answers, in the order they were queued: up to a window of
/// flights.
pub type Ids = Vec<u16, { WINDOW * FLIGHT_MAX }>;

/// Bytes ahead of a queued record's body: its id, then its type.
const ENTRY_HEADER: usize = 3;

/// One flight: its payload as it went out, the ids in it, and its wait for an ack.
struct Flight {
    payload: [u8; PAYLOAD_MAX],
    len: usize,
    ids: Vec<u16, FLIGHT_MAX>,
    tag: FlightTag,
    /// Due to go out: packed and not yet sent, or a resend is due.
    due: bool,
    /// The transfer that carried it last.
    seq: Option<u16>,
    /// When the try now running began: the first send since it was packed or last timed out.
    since_ms: Option<u64>,
    /// Timeouts so far.
    timeouts: u8,
}

impl Flight {
    const EMPTY: Self = Self {
        payload: [0; PAYLOAD_MAX],
        len: 0,
        ids: Vec::new(),
        tag: FlightTag { number: 0, sync: false },
        due: false,
        seq: None,
        since_ms: None,
        timeouts: 0,
    };
}

/// One slot's records, `B` bytes of them queued behind the flights out.
pub struct Downlink<const B: usize> {
    queue: FrameRing<B>,
    /// A ring of the flights out, the oldest at `head`.
    flights: [Flight; WINDOW],
    head: usize,
    out: usize,
    /// The number of the flight [`Downlink::fill`] put in the last transfer.
    filled: Option<u8>,
    /// The next flight's number.
    number: u8,
    /// Whether the node's numbering follows this window's. Until it does, the next flight
    /// carries [`SYNC`](super::SYNC): the first to this slot, and the first after giving up. False
    /// at rest, so a static of these is all zeros.
    synced: bool,
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
            flights: [Flight::EMPTY; WINDOW],
            head: 0,
            out: 0,
            filled: None,
            number: 0,
            synced: false,
            refused: 0,
        }
    }

    /// Queue record `id` of type `kind`. `body` is a [`ToNodeBody`](crate::link::ToNodeBody),
    /// which one transfer always carries: [`RECORD_MAX`] is [`MAX_NODE_DOWN`], checked at build.
    ///
    /// # Errors
    /// [`Undelivered::QueueFull`] when the queue has no room. Nothing is queued then.
    pub fn push(&mut self, id: u16, kind: u8, body: &[u8]) -> Result<(), Undelivered> {
        debug_assert!(body.len() <= MAX_NODE_DOWN, "a {}-byte body for a node", body.len());
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

    /// Whether the next transfer to this slot should carry records: a flight is due, or the
    /// window has room and records are queued.
    #[must_use]
    pub fn has_work(&self) -> bool {
        self.due().is_some() || (self.can_open() && !self.queue.is_empty())
    }

    /// Whether a flight that has gone out waits for its ack.
    #[must_use]
    pub fn is_waiting(&self) -> bool {
        (0..self.out).any(|k| self.flight(k).seq.is_some())
    }

    /// When [`Downlink::poll`] next has something to say.
    #[must_use]
    pub fn deadline_ms(&self) -> Option<u64> {
        if self.out == 0 {
            return None;
        }
        self.flight(0).since_ms.map(|since| since + ACK_TIMEOUT_MS)
    }

    /// Seal the next transfer in `buf` under `header`, tagged with its flight when it carries
    /// one, and return its payload's length: the oldest flight due, a new flight from the queue
    /// when the window has room, or 0.
    pub fn fill(&mut self, buf: &mut [u8; TRANSFER_LEN], header: Header) -> usize {
        if self.due().is_none() && self.can_open() && !self.queue.is_empty() {
            self.open();
        }
        let Some(k) = self.due() else {
            self.filled = None;
            seal(buf, header, 0);
            return 0;
        };
        let flight = self.flight(k);
        let (len, tag) = (flight.len, flight.tag);
        buf[HEADER_LEN..HEADER_LEN + len].copy_from_slice(&flight.payload[..len]);
        self.filled = Some(tag.number);
        seal(buf, header.with_flight(tag), len);
        len
    }

    /// The transfer [`Downlink::fill`] filled went out as `seq` at `now_ms`. Nothing when it
    /// carried no flight, or [`Downlink::on_header`] has since delivered it. It stays due when a
    /// go-back since has made a flight ahead of it due, which the node drops it behind.
    pub fn sent(&mut self, seq: u16, now_ms: u64) {
        let Some(number) = self.filled.take() else { return };
        let Some(k) = (0..self.out).find(|&k| self.flight(k).tag.number == number) else { return };
        let behind_due = (0..k).any(|j| self.flight(j).due);
        let flight = self.flight_mut(k);
        flight.due = behind_due;
        flight.seq = Some(seq);
        flight.since_ms.get_or_insert(now_ms);
    }

    /// Read a header from the node that arrived in the P4's transfer `carried_in`. Returns the
    /// ids of the flights it acknowledges, and makes the window due again from the oldest flight
    /// that went out before it unacknowledged.
    ///
    /// An ack of `carried_in` itself is ignored: the node sealed that header before it saw this
    /// side's half of the transfer, so such an ack is stale, from a P4 that restarted. A stale ack
    /// can still match a later transfer whose records the node failed to take; that is rare, and
    /// accepted.
    pub fn on_header(&mut self, header: &Header, carried_in: u16) -> Option<Ids> {
        let ack = header.acked().filter(|&ack| ack != carried_in);
        let acked = ack.and_then(|ack| (0..self.out).find(|&k| self.flight(k).seq == Some(ack)));
        let delivered = acked.map(|k| {
            let mut ids = Ids::new();
            for _ in 0..=k {
                // Fits: a window of flights.
                let _ = ids.extend_from_slice(&self.retire());
            }
            ids
        });
        if self.out > 0 && self.flight(0).seq.is_some_and(|seq| seq != carried_in) {
            self.go_back();
        }
        delivered
    }

    /// Mark the window due again when the oldest flight's timeout passes, and return every
    /// flight's ids once its last try has timed out.
    pub fn poll(&mut self, now_ms: u64) -> Option<Ids> {
        let deadline = self.deadline_ms()?;
        if now_ms < deadline {
            return None;
        }
        let oldest = self.flight_mut(0);
        oldest.timeouts = oldest.timeouts.saturating_add(1);
        oldest.since_ms = None;
        if oldest.timeouts < SEND_TRIES {
            self.go_back();
            return None;
        }
        let mut ids = Ids::new();
        while self.out > 0 {
            // Fits: a window of flights.
            let _ = ids.extend_from_slice(&self.retire());
        }
        self.synced = false;
        Some(ids)
    }

    /// Empty the window and the queue, as for a slot whose node has gone, giving `each` every
    /// id. The next flight carries [`SYNC`](super::SYNC).
    pub fn drain(&mut self, mut each: impl FnMut(u16)) {
        while self.out > 0 {
            for id in self.retire() {
                each(id);
            }
        }
        let mut entry = [0u8; ENTRY_HEADER + RECORD_MAX];
        while !self.queue.is_empty() {
            self.queue.read(&mut entry);
            each(u16::from_le_bytes([entry[0], entry[1]]));
        }
        self.filled = None;
        self.synced = false;
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

    /// The `k`th flight out, from the oldest.
    fn flight(&self, k: usize) -> &Flight {
        &self.flights[(self.head + k) % WINDOW]
    }

    fn flight_mut(&mut self, k: usize) -> &mut Flight {
        &mut self.flights[(self.head + k) % WINDOW]
    }

    /// The oldest flight due, from the oldest out.
    fn due(&self) -> Option<usize> {
        (0..self.out).find(|&k| self.flight(k).due)
    }

    /// Whether a new flight may go out: the window has room, and holds no [`SYNC`](super::SYNC)
    /// flight, which goes alone.
    fn can_open(&self) -> bool {
        self.out < WINDOW && (self.out == 0 || !self.flight(0).tag.sync)
    }

    /// Every flight out is due again, in order.
    fn go_back(&mut self) {
        for k in 0..self.out {
            self.flight_mut(k).due = true;
        }
    }

    /// Pack a new flight from the queue, as many records as fit, behind the flights out.
    fn open(&mut self) {
        let mut scratch = [0u8; TRANSFER_LEN];
        let mut packer = Packer::new(&mut scratch);
        let mut ids = Vec::new();
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
        if ids.is_empty() {
            return;
        }
        let len = packer.len();
        let tag = FlightTag { number: self.number, sync: !self.synced };
        self.number = forward(self.number);
        self.synced = true;
        let k = self.out;
        self.out += 1;
        let flight = self.flight_mut(k);
        flight.payload[..len].copy_from_slice(&scratch[HEADER_LEN..HEADER_LEN + len]);
        flight.len = len;
        flight.ids = ids;
        flight.tag = tag;
        flight.due = true;
        flight.seq = None;
        flight.since_ms = None;
        flight.timeouts = 0;
    }

    /// Take the oldest flight out of the window, returning its ids.
    fn retire(&mut self) -> Vec<u16, FLIGHT_MAX> {
        let flight = self.flight_mut(0);
        flight.len = 0;
        flight.due = false;
        flight.seq = None;
        let ids = core::mem::take(&mut flight.ids);
        self.head = (self.head + 1) % WINDOW;
        self.out -= 1;
        ids
    }
}
