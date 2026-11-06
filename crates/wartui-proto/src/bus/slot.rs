use core::fmt;

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::NodeBoard;

/// Buses a P4 drives: A and B.
pub const BUSES: u8 = 2;
/// Slots on one bus, one per CS line: seven on the 14-node carrier ([`NodeBoard`]).
pub const PER_BUS: u8 = 7;
/// Slots across both buses: the most nodes one bridge drives.
pub const MAX_SLOTS: usize = (BUSES * PER_BUS) as usize;

/// Where a node sits on the node bus: `bus * PER_BUS + index`, bus A 0 and B 1, index 0 to
/// [`PER_BUS`] − 1.
///
/// The text form is the bus letter and the index from 1: `A1` to `A7`, `B1` to `B7`. On the
/// host link it is the one byte, and a byte naming no slot fails the decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Slot(u8);

/// A byte or text that names no [`Slot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotError;

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a node-bus slot: A1 to A7 or B1 to B7")
    }
}

impl Slot {
    /// Slot `index` of `bus` (0 for A, 1 for B), or `None` past either range.
    #[must_use]
    pub const fn new(bus: u8, index: u8) -> Option<Self> {
        if bus < BUSES && index < PER_BUS { Some(Self(bus * PER_BUS + index)) } else { None }
    }

    /// The slot a link byte names, or `None` past [`MAX_SLOTS`].
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        if (byte as usize) < MAX_SLOTS { Some(Self(byte)) } else { None }
    }

    /// The link byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }

    /// The bus, 0 for A and 1 for B.
    #[must_use]
    pub const fn bus(self) -> u8 {
        self.0 / PER_BUS
    }

    /// The CS line on its bus, from 0.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.0 % PER_BUS
    }

    /// Read the text form, `A1` to `B7`, either case.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let &[letter, digit] = text.as_bytes() else { return None };
        let bus = match letter.to_ascii_uppercase() {
            b'A' => 0,
            b'B' => 1,
            _ => return None,
        };
        let index = digit.checked_sub(b'1')?;
        Self::new(bus, index)
    }
}

impl TryFrom<u8> for Slot {
    type Error = SlotError;

    fn try_from(byte: u8) -> Result<Self, SlotError> {
        Self::from_u8(byte).ok_or(SlotError)
    }
}

impl From<Slot> for u8 {
    fn from(slot: Slot) -> u8 {
        slot.0
    }
}

/// `A1` to `B7`.
impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let letter = if self.bus() == 0 { 'A' } else { 'B' };
        write!(f, "{letter}{}", self.index() + 1)
    }
}
