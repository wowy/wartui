//! The wartui-carrier's I/O expander as the P4 reads it at boot: which carrier this is, and
//! which nodes to power. The firmware does the I2C around [`decide`].
//!
//! The expander sits at [`ADDRESS`] with sixteen ports, and driving a port high powers its
//! node ([`port`]). Each enable is pulled down on the board, so a port that is an input reads
//! low where a node socket is wired and high where it is spare. With every port an input, the
//! inputs name the carrier ([`ports`]).
//!
//! **A P4 boot leaves running nodes running.** The expander is powered from the P4's 3V3, which
//! a software reset does not drop, so it keeps its registers through one. Only this firmware
//! writes them, and it makes exactly one carrier's node ports outputs. So:
//!
//! - **Warm**: the config registers show one carrier's node ports as outputs. Nodes already on
//!   stay on, and any node port whose output is low is powered, since a previous life may have
//!   died mid-cycle.
//! - **Cold**: anything else. The firmware makes every port an input, which turns every node
//!   off, reads the inputs, writes the outputs low, makes only the node ports outputs, and powers
//!   every node. A power-up leaves every port an input with its output high, so every node starts
//!   off.
//! - **Broken**: inputs that name no carrier. Nothing is powered and no slot is driven.
//!
//! As a 16-bit value, a register pair is `port1 << 8 | port0`.

use crate::bus::{NodeBoard, Slot};

/// The expander's I2C address.
pub const ADDRESS: u8 = 0x20;
/// Input registers, port 0 then port 1.
pub const REG_INPUT: u8 = 0;
/// Output registers.
pub const REG_OUTPUT: u8 = 2;
/// Configuration registers: a set bit makes its port an input.
pub const REG_CONFIG: u8 = 6;
/// The configuration with every port an input: the expander's power-on state.
pub const ALL_INPUTS: u16 = 0xFFFF;

/// The expander port bit that switches `slot`'s node: bus A slot *i* is bit 15 − *i* (P17
/// down), bus B slot *i* is bit *i* (P00 up).
#[must_use]
pub const fn port(slot: Slot) -> u16 {
    let i = slot.index();
    if slot.bus() == 0 { 1 << (15 - i) } else { 1 << i }
}

/// The node ports `board` wires: none on the bench.
#[must_use]
pub fn ports(board: NodeBoard) -> u16 {
    if !board.is_carrier() {
        return 0;
    }
    board.slots().fold(0, |bits, slot| bits | port(slot))
}

/// The carriers, smallest first.
const CARRIERS: [NodeBoard; 3] = [NodeBoard::Carrier6, NodeBoard::Carrier10, NodeBoard::Carrier14];

/// What the expander's registers say at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// A P4 reset under running nodes: power the node ports in `power`, which are those whose
    /// output is low.
    Warm {
        /// The carrier the config registers name.
        board: NodeBoard,
        /// Node ports to power.
        power: u16,
    },
    /// A cold start: set up `board`'s node ports and power every node.
    Cold {
        /// The carrier the inputs name.
        board: NodeBoard,
    },
    /// The inputs name no carrier.
    Broken,
}

impl Decision {
    /// The node ports to power, staggered.
    #[must_use]
    pub fn power(self) -> u16 {
        match self {
            Self::Warm { power, .. } => power,
            Self::Cold { board } => ports(board),
            Self::Broken => 0,
        }
    }
}

/// The carrier whose node ports, and only those, `config` makes outputs: what this firmware
/// leaves behind, so a warm boot. The firmware reads the inputs only when this is `None`.
#[must_use]
pub fn warm_board(config: u16) -> Option<NodeBoard> {
    CARRIERS.into_iter().find(|&board| config == !ports(board))
}

/// Decide from the registers: `config` and `output` as found, and `input` as read once every port
/// is an input. `input` is read only on the cold path, so a warm decision ignores it.
///
/// Only the node ports' inputs are compared. The two ports no carrier wires to a node (P07 and
/// P10) carry nothing a variant differs in.
#[must_use]
pub fn decide(config: u16, output: u16, input: u16) -> Decision {
    if let Some(board) = warm_board(config) {
        return Decision::Warm { board, power: ports(board) & !output };
    }
    let wired = ports(NodeBoard::Carrier14);
    CARRIERS
        .into_iter()
        .find(|&board| input & wired == !ports(board) & wired)
        .map_or(Decision::Broken, |board| Decision::Cold { board })
}
