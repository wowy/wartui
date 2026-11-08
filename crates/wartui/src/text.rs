//! Number and label text shared by the subcommands' reports and the TUI.

use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::mac::{self, Mac};

/// `n` with a comma between each group of three digits.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `buffer full` and the non-zero counts of sightings a full pending buffer refused, or
/// `None` when both are zero. The TUI footer prints the same text.
pub(crate) fn buffer_full(wifi: u64, ble: u64) -> Option<String> {
    let figures: Vec<String> = [("wifi", wifi), ("ble", ble)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(kind, n)| format!("{kind} {}", thousands(n)))
        .collect();
    (!figures.is_empty()).then(|| format!("buffer full {}", figures.join("  ")))
}

/// The slot names `board` takes, as a range: `N1 to N10` on a carrier, `none` with no carrier.
pub(crate) fn slot_range(board: NodeBoard) -> String {
    match (board.slots().next(), board.slots().last()) {
        (Some(first), Some(last)) => format!("{} to {}", board.label(first), board.label(last)),
        _ => "none".to_owned(),
    }
}

/// The slot an operator's `name` means on `board`, or why it is refused with the names the board
/// does take.
pub(crate) fn slot_named(board: NodeBoard, name: &str) -> Result<Slot, String> {
    board.parse_label(name).ok_or_else(|| {
        format!("{name:?} is not a slot on this {board}: name one of {}", slot_range(board))
    })
}

/// The slot `name` means on a carrier `board`, or why it is refused: `no_carrier` for a bridge
/// with no carrier, and otherwise the names the board does take.
pub(crate) fn carrier_slot_named(
    board: NodeBoard,
    name: &str,
    no_carrier: &str,
) -> Result<Slot, String> {
    if !board.is_carrier() {
        return Err(no_carrier.to_owned());
    }
    slot_named(board, name)
}

/// What the operator calls a node: its slot's label, which is on the carrier for them to see, or
/// the tail of its address for a node with no slot, which has nothing else to go by.
pub(crate) fn node_name(board: NodeBoard, slot: Option<Slot>, mac: &Mac) -> String {
    slot.map_or_else(|| mac::short(mac).to_string(), |slot| board.label(slot).to_string())
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::mac::Mac;

    use super::{node_name, slot_named, slot_range, thousands};

    const MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];

    #[test]
    fn node_name_gives_slot_label_when_node_has_slot() {
        let slot = Slot::new(1, 0).expect("B1");
        assert_eq!(node_name(NodeBoard::Carrier14, Some(slot), &MAC), "N8");
    }

    #[test]
    fn node_name_gives_address_tail_when_node_has_no_slot() {
        assert_eq!(node_name(NodeBoard::Carrier14, None, &MAC), "57:84");
    }

    #[test]
    fn slot_named_refuses_name_with_range_when_board_does_not_take_it() {
        assert_eq!(slot_named(NodeBoard::Carrier14, "n14"), Ok(Slot::new(1, 6).expect("B7")));
        let refused = slot_named(NodeBoard::Carrier14, "N15").expect_err("past the board");
        assert!(refused.contains("N1 to N14"), "{refused}");
    }

    #[test]
    fn slot_range_names_first_and_last_slot_when_board_is_a_carrier() {
        assert_eq!(slot_range(NodeBoard::Carrier6), "N1 to N6");
        assert_eq!(slot_range(NodeBoard::Carrier14), "N1 to N14");
    }

    #[test]
    fn slot_range_names_none_when_there_is_no_carrier() {
        assert_eq!(slot_range(NodeBoard::NoCarrier), "none");
    }

    #[test]
    fn thousands_separates_groups_when_number_exceeds_999() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
    }
}
