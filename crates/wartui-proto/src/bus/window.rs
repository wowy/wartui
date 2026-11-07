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
//! and the one after apart. Neither end can learn where the other's numbering stands, so a
//! [`SYNC`] flight sets it: the node takes it whatever its number. A node that has taken no
//! flight since boot takes only a [`SYNC`] flight, since a flight taken on its number alone may
//! be the second of two whose first it never saw, and its ack would deliver both. Its headers
//! carry no ack, and the P4 answers any header without one by resending its oldest flight out
//! with [`SYNC`]. It also tags its first flight to a slot, and its first after giving one up. A
//! [`SYNC`] flight goes alone, so a resend of it taken twice arrives next to itself, never after
//! the flight behind it.

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
    /// The next flight: queue it, then [`FlightTaker::commit`] it and acknowledge it.
    Queue,
    /// A resend of the flight last taken: acknowledge it, and do not queue it again.
    Repeat,
    /// A flight to take, with no room for it: drop it unacknowledged.
    Full,
    /// A flight past one the node has not taken, or one without [`SYNC`] before the first taken
    /// since boot: drop it unacknowledged.
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
    /// `room` or not. Nothing moves until [`FlightTaker::commit`].
    #[must_use]
    pub fn take(&self, header: &Header, room: bool) -> Take {
        let tag = header.flight();
        if !tag.sync {
            let Some(expected) = self.expected else { return Take::OutOfOrder };
            if tag.number == back(expected) {
                return Take::Repeat;
            }
            if tag.number != expected {
                return Take::OutOfOrder;
            }
        }
        if room { Take::Queue } else { Take::Full }
    }

    /// The flight `header` carries, which [`FlightTaker::take`] answered [`Take::Queue`], is in
    /// the inbox: expect the next number. A flight that never reached the inbox is not
    /// committed, so its resend reads as the next flight rather than a [`Take::Repeat`].
    pub fn commit(&mut self, header: &Header) {
        self.expected = Some(forward(header.flight().number));
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
