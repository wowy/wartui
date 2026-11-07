//! The window toward a node: the P4 numbers each flight, and the node takes them strictly in
//! number order.
//!
//! The P4 sends a node's next flight in the transfer that fetches the previous one's ack, so up
//! to [`WINDOW`] flights are out at once ([`Downlink`](super::Downlink)). Delivery stays in order,
//! because the host's update session counts on it: a node never takes a flight whose predecessor
//! it did not take. One that misses a flight, to a full inbox or a transfer that failed its
//! checks, drops every later one unacknowledged until the missing one arrives again, and the P4
//! resends from it (go-back-N). A resend of the flight just taken, whose ack the P4 missed, is
//! acknowledged and not taken again.
//!
//! Numbers run mod [`FLIGHT_NUMBERS`], which tells the node's last flight, the one it expects,
//! and the one after apart. A node that has taken nothing since boot takes whatever comes. A P4
//! has no such way to learn where a node stands, so its first flight to a slot, and its first
//! after giving one up, carries [`SYNC`]: the node takes it whatever its number. A [`SYNC`]
//! flight goes alone, so a resend of it taken twice arrives next to itself, never after the
//! flight behind it.

use super::{FLIGHT, Header, SYNC};

/// Flights out toward a node at once.
pub const WINDOW: usize = 2;

/// Flight numbers, which wrap. Twice the window, so the flight last taken, the one expected and
/// the one after never share a number.
pub const FLIGHT_NUMBERS: u8 = 4;

/// Bits [`FLIGHT`] sits above.
const FLIGHT_SHIFT: u8 = 1;

const _: () = assert!(FLIGHT_NUMBERS as usize > WINDOW, "numbers tell a window's flights apart");
const _: () = assert!((FLIGHT >> FLIGHT_SHIFT) + 1 == FLIGHT_NUMBERS, "FLIGHT holds a number");

/// What a record-carrying transfer from the P4 says about its flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlightTag {
    /// The flight's number, below [`FLIGHT_NUMBERS`].
    pub number: u8,
    /// The node takes this flight whatever it expects.
    pub sync: bool,
}

impl FlightTag {
    /// The tag's bits in the flags byte.
    #[must_use]
    pub const fn flags(self) -> u8 {
        ((self.number << FLIGHT_SHIFT) & FLIGHT) | if self.sync { SYNC } else { 0 }
    }

    /// The tag in a flags byte.
    #[must_use]
    pub const fn from_flags(flags: u8) -> Self {
        Self { number: (flags & FLIGHT) >> FLIGHT_SHIFT, sync: flags & SYNC != 0 }
    }
}

impl Header {
    /// This header, tagged with `tag`'s flight.
    #[must_use]
    pub const fn with_flight(self, tag: FlightTag) -> Self {
        Self { flags: (self.flags & !(FLIGHT | SYNC)) | tag.flags(), ..self }
    }

    /// The flight a record-carrying transfer from the P4 carries.
    #[must_use]
    pub const fn flight(&self) -> FlightTag {
        FlightTag::from_flags(self.flags)
    }
}

/// What a node does with a record-carrying transfer from the P4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// The next flight: queue it and acknowledge it.
    Queue,
    /// A resend of the flight last taken: acknowledge it, and do not queue it again.
    Repeat,
    /// A flight to take, with no room for it: drop it unacknowledged.
    Full,
    /// A flight past one the node has not taken: drop it unacknowledged.
    OutOfOrder,
}

/// A node's side of the window: which flight it takes next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlightTaker {
    /// The number expected next, or `None` before the first flight taken since boot.
    expected: Option<u8>,
}

impl FlightTaker {
    /// Nothing taken since boot.
    #[must_use]
    pub const fn new() -> Self {
        Self { expected: None }
    }

    /// Decide on a record-carrying transfer from the P4 with `header`, when the inbox has
    /// `room` or not. [`Take::Queue`] moves on to the next number; nothing else does.
    pub fn take(&mut self, header: &Header, room: bool) -> Take {
        let tag = header.flight();
        if let Some(expected) = self.expected
            && !tag.sync
        {
            if tag.number == back(expected) {
                return Take::Repeat;
            }
            if tag.number != expected {
                return Take::OutOfOrder;
            }
        }
        if !room {
            return Take::Full;
        }
        self.expected = Some(forward(tag.number));
        Take::Queue
    }
}

/// The number after `number`.
#[must_use]
pub(super) const fn forward(number: u8) -> u8 {
    (number + 1) % FLIGHT_NUMBERS
}

/// The number before `number`.
const fn back(number: u8) -> u8 {
    (number + FLIGHT_NUMBERS - 1) % FLIGHT_NUMBERS
}
