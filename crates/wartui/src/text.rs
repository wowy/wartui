//! Number and label text shared by the subcommands' reports and the TUI.

use wartui_proto::bus::{BUSES, NodeBoard, Slot};

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

/// The slot names `board` takes, as a range: `N1 to N10` on a carrier, `A1 to A6 or B1 to B6`
/// on the bench.
pub(crate) fn slot_range(board: NodeBoard) -> String {
    let name = |bus, index| {
        Slot::new(bus, index).map_or_else(String::new, |slot| board.label(slot).to_string())
    };
    let last = board.per_bus() - 1;
    if board.is_carrier() {
        format!("{} to {}", name(0, 0), name(BUSES - 1, last))
    } else {
        format!("{} to {} or {} to {}", name(0, 0), name(0, last), name(1, 0), name(1, last))
    }
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::NodeBoard;

    use super::{slot_range, thousands};

    #[test]
    fn slot_range_names_first_and_last_slot_when_board_is_known() {
        assert_eq!(slot_range(NodeBoard::Bench), "A1 to A6 or B1 to B6");
        assert_eq!(slot_range(NodeBoard::Carrier6), "N1 to N6");
        assert_eq!(slot_range(NodeBoard::Carrier14), "N1 to N14");
    }

    #[test]
    fn thousands_separates_groups_when_number_exceeds_999() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
    }
}
