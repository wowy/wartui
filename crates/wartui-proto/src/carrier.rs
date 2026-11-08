//! The wartui-carrier's I/O expander as the P4 reads it at boot: which carrier this is, and
//! which nodes to power. The firmware does the I2C around [`decide`]. [`DropWatch`] tells the
//! firmware when the carrier's node supply was likely lost.
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
//!
//! **The P4 cannot sense the node supply**, and stays up on its own USB-C while it is out. Its
//! only sign is every present node going absent at once, which [`DropWatch`] looks for. A node
//! in a power-cycle drops for that reason, so its drop does not count, while one still waiting
//! for its cycle has power and counts like any other. Nor does a lone node's drop count: one
//! node has no inrush to stagger, and its drop is no different from a crash or an operator
//! pulling it.

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

/// The node ports `board` wires: none with no carrier, which drives no slot.
#[must_use]
pub fn ports(board: NodeBoard) -> u16 {
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

/// How close together every present node's drops must fall to read as a lost supply. A node is
/// absent about 1.5 s after it loses power.
pub const DROP_WINDOW_MS: u64 = 2_000;

/// Watches which slots hold a node, for every one dropping at once. A mask's bit `n` is slot
/// byte `n` ([`Slot::as_u8`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DropWatch {
    /// The slots present at the last change.
    present: u16,
    /// The slots that dropped since [`Self::since_ms`], cycled ones aside.
    dropped: u16,
    /// When the first of [`Self::dropped`] went.
    since_ms: u64,
    /// Slots whose cycle ended lately, so a late drop is still the cycle's.
    cycled: u16,
    /// Until when [`Self::cycled`] holds.
    cycled_until_ms: u64,
}

impl DropWatch {
    /// Nothing present yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { present: 0, dropped: 0, since_ms: 0, cycled: 0, cycled_until_ms: 0 }
    }

    /// Note that `present` hold a node at `now_ms`, while `cycling` are in a power-cycle now.
    /// Returns the slots that were present before every node dropped, once the last
    /// of them goes within [`DROP_WINDOW_MS`] of the first, and only when there are two or more.
    pub fn note(&mut self, present: u16, cycling: u16, now_ms: u64) -> Option<u16> {
        if now_ms > self.cycled_until_ms {
            self.cycled = 0;
        }
        // A node arriving is no supply loss, and drops further apart are no single event.
        if present & !self.present != 0 || now_ms.saturating_sub(self.since_ms) > DROP_WINDOW_MS {
            self.dropped = 0;
        }
        let gone = self.present & !present & !cycling & !self.cycled;
        self.present = present;
        if gone != 0 {
            if self.dropped == 0 {
                self.since_ms = now_ms;
            }
            self.dropped |= gone;
        }
        if present != 0 {
            return None;
        }
        let dropped = core::mem::take(&mut self.dropped);
        (dropped.count_ones() >= 2).then_some(dropped)
    }

    /// The cycle of `slots` ended at `now_ms`. A node is absent only some time after it loses
    /// power, so a drop within [`DROP_WINDOW_MS`] is still the cycle's.
    pub fn cycled(&mut self, slots: u16, now_ms: u64) {
        if now_ms > self.cycled_until_ms {
            self.cycled = 0;
        }
        self.cycled |= slots;
        self.cycled_until_ms = now_ms.saturating_add(DROP_WINDOW_MS);
    }
}
