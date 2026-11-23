//! Number and label text shared by the subcommands' reports and the TUI.

use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::link::{SdState, SdStatus};
use wartui_proto::mac::{self, Mac};
use wartui_proto::plan::{ChannelSet, SCAN_CHANNELS};

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

/// Where the bridge's microSD card is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SdForm {
    /// `wartui status` and `wartui sd`: a line printed once.
    Line,
    /// The settings modal's row, redrawn while a format runs, beside a tip that says the rest.
    Row,
}

/// `mib` in GB, MiB / 1024 with one decimal: `61.8`.
pub(crate) fn gb(mib: u32) -> String {
    format!("{:.1}", f64::from(mib) / 1024.0)
}

/// The bridge's microSD card, as `form` shows it: `58.9 of 61.8 GB free`, or `no slot` for a
/// bridge with none.
pub(crate) fn sd_text(sd: Option<SdStatus>, form: SdForm) -> String {
    let Some(SdStatus { state, card_mb, free_mb }) = sd else { return "no slot".to_owned() };
    let size = gb(card_mb);
    match (state, form) {
        (SdState::NoCard, _) => "no card".to_owned(),
        (SdState::NoFat32, _) => format!("{size} GB, not FAT32"),
        (SdState::Ready, _) => match free_mb {
            Some(free) => format!("{} of {size} GB free", gb(free)),
            None => format!("{size} GB, FAT32"),
        },
        (SdState::Formatting, SdForm::Line) => "formatting".to_owned(),
        (SdState::Formatting, SdForm::Row) => "formatting…".to_owned(),
        (SdState::Failed, SdForm::Line) => format!("card error ({size} GB)"),
        (SdState::Failed, SdForm::Row) => "card error".to_owned(),
    }
}

/// Format a set as channel numbers, the units every other tool uses.
///
/// Runs collapse to `first-last`, so a contiguous set reads `1-11`.
pub(crate) fn channel_list(set: ChannelSet) -> String {
    let mut out = String::new();
    let mut run: Option<(u8, u8)> = None;
    for idx in set.indices() {
        match run {
            Some((start, end)) if idx == end + 1 => run = Some((start, idx)),
            Some((start, end)) => {
                push_run(&mut out, start, end);
                run = Some((idx, idx));
            }
            None => run = Some((idx, idx)),
        }
    }
    if let Some((start, end)) = run {
        push_run(&mut out, start, end);
    }
    if out.is_empty() { "none".to_owned() } else { out }
}

/// Append one run to `out`: `6`, or `36-48`.
fn push_run(out: &mut String, start: u8, end: u8) {
    use core::fmt::Write as _;
    let first = SCAN_CHANNELS.get(usize::from(start)).copied().unwrap_or(0);
    let last = SCAN_CHANNELS.get(usize::from(end)).copied().unwrap_or(0);
    if !out.is_empty() {
        out.push(',');
    }
    // Writing into a `String` cannot fail.
    let _ = if first == last { write!(out, "{first}") } else { write!(out, "{first}-{last}") };
}

#[cfg(test)]
mod tests {
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::link::{SdState, SdStatus};
    use wartui_proto::mac::Mac;
    use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun};

    use super::{SdForm, channel_list, node_name, sd_text, slot_named, slot_range, thousands};

    const MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];

    /// A "64 GB" card: 63,284 MiB, 61.8 GB.
    fn card(state: SdState, free_mb: Option<u32>) -> Option<SdStatus> {
        Some(SdStatus { state, card_mb: 63_284, free_mb })
    }

    #[test]
    fn sd_text_names_each_state_when_printed_as_a_line() {
        let line = |sd| sd_text(sd, SdForm::Line);
        let no_card = SdStatus { state: SdState::NoCard, card_mb: 0, free_mb: None };
        assert_eq!(line(None), "no slot");
        assert_eq!(line(Some(no_card)), "no card");
        assert_eq!(line(card(SdState::NoFat32, None)), "61.8 GB, not FAT32");
        assert_eq!(line(card(SdState::Ready, Some(60_314))), "58.9 of 61.8 GB free");
        assert_eq!(line(card(SdState::Ready, None)), "61.8 GB, FAT32");
        assert_eq!(line(card(SdState::Formatting, None)), "formatting");
        assert_eq!(line(card(SdState::Failed, None)), "card error (61.8 GB)");
    }

    #[test]
    fn sd_text_shortens_formatting_and_error_when_drawn_as_a_row() {
        let row = |sd| sd_text(sd, SdForm::Row);
        assert_eq!(row(card(SdState::Formatting, None)), "formatting…");
        assert_eq!(row(card(SdState::Failed, None)), "card error");
        // The rest read as the line does.
        assert_eq!(row(card(SdState::Ready, Some(60_314))), "58.9 of 61.8 GB free");
        assert_eq!(row(None), "no slot");
    }

    #[test]
    fn sd_text_rounds_to_one_decimal_when_size_is_not_whole_gb() {
        let sd = Some(SdStatus { state: SdState::NoFat32, card_mb: 30_436, free_mb: None });
        assert_eq!(sd_text(sd, SdForm::Line), "29.7 GB, not FAT32");
    }

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

    #[test]
    fn channel_list_formats_indices_as_channel_numbers_when_given_index_runs() {
        // Indices into SCAN_CHANNELS belong to the wire format, not the screen.
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(0, 0))), "1");
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(0, 10))), "1-11");
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(14, 38))), "36-165");
    }

    #[test]
    fn channel_list_formats_channels_as_compact_runs_when_channel_set_is_scattered() {
        assert_eq!(channel_list(ChannelPool::Us.channels()), "1-11,36-165");
        let mut comb = ChannelSet::empty();
        for idx in [0, 2, 4, 14, 15] {
            comb.insert(idx);
        }
        assert_eq!(channel_list(comb), "1,3,5,36-40");
        assert_eq!(channel_list(ChannelSet::empty()), "none");
    }
}
