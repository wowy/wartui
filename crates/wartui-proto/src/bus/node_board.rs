use core::fmt;

use serde::{Deserialize, Serialize};

use super::slot::{BUSES, Slot};

/// What carries a P4's nodes, which decides the slots it drives and the names an operator
/// reads for them.
///
/// On the bench, nodes are wired straight to the P4 and named by their bus slot, `A1` to `B6`.
/// The wartui-carrier comes in three sizes, and its silk names its sockets `N1` to `Nn`: bus A
/// first, then bus B. So `B1` is `N4` on the six-node carrier, `N6` on the ten and `N8` on the
/// fourteen. The P4 reads which one it sits on at boot and says so in `Ready`.
///
/// On the host link it is one byte, and a byte naming no board fails the decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum NodeBoard {
    /// No carrier: nodes wired to the P4's six CS pins per bus.
    Bench,
    /// The six-node carrier: three per bus.
    Carrier6,
    /// The ten-node carrier: five per bus.
    Carrier10,
    /// The fourteen-node carrier: seven per bus.
    Carrier14,
}

/// A byte that names no [`NodeBoard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeBoardError;

impl fmt::Display for NodeBoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a node board")
    }
}

impl NodeBoard {
    /// Every board, in link-byte order.
    pub const ALL: [Self; 4] = [Self::Bench, Self::Carrier6, Self::Carrier10, Self::Carrier14];

    /// The link byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Bench => 0,
            Self::Carrier6 => 1,
            Self::Carrier10 => 2,
            Self::Carrier14 => 3,
        }
    }

    /// The board a link byte names, or `None`.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Bench),
            1 => Some(Self::Carrier6),
            2 => Some(Self::Carrier10),
            3 => Some(Self::Carrier14),
            _ => None,
        }
    }

    /// Slots this board drives on each bus, from slot 1.
    #[must_use]
    pub const fn per_bus(self) -> u8 {
        match self {
            Self::Bench => 6,
            Self::Carrier6 => 3,
            Self::Carrier10 => 5,
            Self::Carrier14 => 7,
        }
    }

    /// Slots this board drives across both buses.
    #[must_use]
    pub const fn slot_count(self) -> u8 {
        BUSES * self.per_bus()
    }

    /// Whether this is a wartui-carrier, which switches its nodes' power.
    #[must_use]
    pub const fn is_carrier(self) -> bool {
        !matches!(self, Self::Bench)
    }

    /// Whether this board drives `slot`.
    #[must_use]
    pub const fn drives(self, slot: Slot) -> bool {
        slot.index() < self.per_bus()
    }

    /// Every slot this board drives, bus A first.
    pub fn slots(self) -> impl Iterator<Item = Slot> {
        (0..BUSES).flat_map(move |bus| (0..self.per_bus()).filter_map(move |i| Slot::new(bus, i)))
    }

    /// What an operator calls `slot`: `A1` to `B6` on the bench, `N1` to `Nn` on a carrier. A
    /// slot this board does not drive keeps its bus name.
    #[must_use]
    pub const fn label(self, slot: Slot) -> SlotLabel {
        SlotLabel { board: self, slot }
    }

    /// The slot an operator's name means, either case, or `None` for a name this board does not
    /// take: on a carrier only the `N` names it drives, and on the bench only `A1` to `B6`.
    #[must_use]
    pub fn parse_label(self, text: &str) -> Option<Slot> {
        if !self.is_carrier() {
            return Slot::parse(text).filter(|&slot| self.drives(slot));
        }
        let digits = text.strip_prefix(['N', 'n'])?;
        // `u8::from_str` takes a sign and leading zeros, which no silk name has.
        if digits.is_empty()
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let n: u8 = digits.parse().ok()?;
        let k = n.checked_sub(1).filter(|&k| k < self.slot_count())?;
        Slot::new(k / self.per_bus(), k % self.per_bus())
    }

    /// The name `wartui status` prints and a capture stores: `bench`, `carrier-6`, `carrier-10`
    /// or `carrier-14`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bench => "bench",
            Self::Carrier6 => "carrier-6",
            Self::Carrier10 => "carrier-10",
            Self::Carrier14 => "carrier-14",
        }
    }
}

impl TryFrom<u8> for NodeBoard {
    type Error = NodeBoardError;

    fn try_from(byte: u8) -> Result<Self, NodeBoardError> {
        Self::from_u8(byte).ok_or(NodeBoardError)
    }
}

impl From<NodeBoard> for u8 {
    fn from(board: NodeBoard) -> u8 {
        board.as_u8()
    }
}

impl fmt::Display for NodeBoard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A slot as an operator names it on one board ([`NodeBoard::label`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotLabel {
    board: NodeBoard,
    slot: Slot,
}

impl fmt::Display for SlotLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { board, slot } = *self;
        if board.is_carrier() && board.drives(slot) {
            write!(f, "N{}", slot.bus() * board.per_bus() + slot.index() + 1)
        } else {
            write!(f, "{slot}")
        }
    }
}
