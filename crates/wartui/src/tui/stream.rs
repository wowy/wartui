//! The unique counts are drawn big above the table, because they are the figures a
//! driver checks most often. A pane too short to spare the rows, or too narrow for both,
//! falls back to smaller text so the table keeps room.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use wartui_core::engine::{Snapshot, TailEntry};
use wartui_proto::mac;
// Shared with the bridge's panel, so the two cannot show different numbers for one
// estimate.
use wartui_core::panel::approx;
use wartui_proto::air::RecordKind;

use super::bignum::big;
use super::format::clock;

/// Rows the big counts take.
const TOTALS_HEIGHT: u16 = 3;

pub(super) fn draw_stream(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let header = Row::new(["time", "node", "bssid", "ch", "rssi", "ssid"])
        .style(Style::new().add_modifier(Modifier::BOLD));

    let aps = approx(snapshot.unique_wifi_aps);
    let ble = approx(snapshot.unique_ble_aps);
    let records = snapshot.counters.observations;

    // The big counts need the table to keep its header and two rows.
    let inner = Block::bordered().inner(area);
    let tall = inner.height >= TOTALS_HEIGHT + 3;
    let (title, totals_height) = if tall {
        (format!(" {records} records "), TOTALS_HEIGHT)
    } else {
        (format!(" unique APs ~{aps} — unique BLE ~{ble} ({records} total records) "), 0)
    };
    let block = Block::bordered().title(title);
    let [totals, table] =
        Layout::vertical([Constraint::Length(totals_height), Constraint::Min(0)]).areas(inner);

    // Newest last, and only as many as fit. The tail holds up to 200, but the pane
    // shows a couple of dozen.
    let visible = usize::from(table.height.saturating_sub(1));
    let rows: Vec<Row<'_>> =
        snapshot.tail.iter().rev().take(visible).rev().map(observation_row).collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(17),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Min(10),
    ];
    frame.render_widget(block, area);
    if tall {
        frame.render_widget(Paragraph::new(totals_lines(&aps, &ble, totals.width)), totals);
    }
    frame.render_widget(Table::new(rows, widths).header(header), table);
}

/// The big AP and BLE counts, side by side. The BLE count is small text beside the AP
/// count when both do not fit in `width`.
fn totals_lines(aps: &str, ble: &str, width: u16) -> Vec<Line<'static>> {
    let ap = big_count(aps, kind_style(RecordKind::Wifi), ["", " unique APs", ""]);
    let full: Vec<Line<'static>> = ap
        .iter()
        .zip(big_count(ble, kind_style(RecordKind::Ble), ["", " unique BLE", ""]))
        .map(|(ap, ble)| {
            let mut spans = ap.clone();
            spans.push(Span::raw("   "));
            spans.extend(ble);
            Line::from(spans)
        })
        .collect();
    if full.iter().all(|line| line.width() <= usize::from(width)) {
        return full;
    }
    let ble = format!(" BLE ~{ble}");
    let narrow = big_count(aps, kind_style(RecordKind::Wifi), [" unique APs", &ble, ""]);
    narrow.into_iter().map(Line::from).collect()
}

/// One count's three rows: the `~` estimate marker, the big digits in `style`, then
/// `labels` beside them, padded to one width.
fn big_count(text: &str, style: Style, labels: [&str; 3]) -> Vec<Vec<Span<'static>>> {
    let style = style.add_modifier(Modifier::BOLD);
    let pad = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    big(text)
        .into_iter()
        .zip(labels)
        .enumerate()
        .map(|(i, (glyphs, label))| {
            vec![
                Span::raw(if i == 1 { "~" } else { " " }),
                Span::styled(glyphs, style),
                Span::raw(format!("{label:<pad$}")),
            ]
        })
        .collect()
}

/// The colour a record's kind is drawn in.
fn kind_style(kind: RecordKind) -> Style {
    match kind {
        RecordKind::Wifi => Style::new().fg(Color::Green),
        RecordKind::Ble => Style::new().fg(Color::Blue),
    }
}

fn observation_row(entry: &TailEntry) -> Row<'static> {
    let kind = kind_style(entry.kind);
    let ssid = if entry.ssid.is_empty() {
        let text = if entry.kind == RecordKind::Wifi { "<hidden>" } else { "<n/a>" };
        Span::styled(text, Style::new().fg(Color::DarkGray))
    } else {
        Span::raw(entry.ssid.clone())
    };
    Row::new(vec![
        Cell::from(clock(entry.rx_at_ms)),
        // The same octets the fleet table names the node by.
        Cell::from(mac::short(&entry.node_mac).to_string()),
        Cell::from(mac::full(&entry.bssid).to_string()).style(kind),
        Cell::from(if entry.channel == 0 { "—".to_owned() } else { entry.channel.to_string() }),
        Cell::from(entry.rssi.to_string()),
        Cell::from(Line::from(ssid)),
    ])
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    fn screen(snapshot: &Snapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, &mut Ui::default())).expect("drawing");
        terminal.backend().to_string()
    }

    #[test]
    fn view_displays_big_unique_counts_when_stream_pane_is_large() {
        let rendered = screen(&busy(), 150, 40);
        for row in big("32").iter().chain(big("8").iter()) {
            assert!(rendered.contains(row.as_str()), "{row:?} in\n{rendered}");
        }
        assert!(rendered.contains("unique APs"), "{rendered}");
        assert!(rendered.contains("unique BLE"), "{rendered}");
        assert!(rendered.contains("800 records"), "{rendered}");
    }

    #[test]
    fn view_displays_ble_count_as_text_when_both_big_counts_do_not_fit() {
        // Five-glyph figures for both, so the pair is wider than the stacked pane.
        let mut snapshot = busy();
        snapshot.unique_wifi_aps = 1_230_000;
        snapshot.unique_ble_aps = 12_345;
        let rendered = screen(&snapshot, 50, 40);
        for row in big("1.23M") {
            assert!(rendered.contains(row.trim_end()), "{row:?} in\n{rendered}");
        }
        assert!(rendered.contains("BLE ~12.3k"), "{rendered}");
        assert!(!rendered.contains("unique BLE"), "{rendered}");
    }

    #[test]
    fn view_displays_one_line_title_when_stream_pane_is_short() {
        let rendered = screen(&busy(), 150, 12);
        assert!(rendered.contains("unique APs ~32"), "{rendered}");
        assert!(!rendered.contains("800 records "), "{rendered}");
    }
}
