//! One block per node, across the width of the screen, because the figures an operator checks
//! most are per node and a laptop has the columns for all fourteen at once. Blocks never stretch
//! vertically: a screen too short for every row scrolls them, and the strip above takes the rest.
//!
//! A block's border carries its state's color, and its last line still spells the state out,
//! because color alone is not the signal: a terminal without color, or an operator who cannot
//! tell red from green, reads the word.
//!
//! The channel lines show each node's share of the bridge's plan, colored by whether the node
//! holds it. Only the node's heartbeat, carrying the epoch it holds, shows it took the share
//! (`wartui_app::view::ShareShown`'s `pending`), so nothing here waits on the bridge's own delivery.
//!
//! | State | Wi-Fi share | Bluetooth node |
//! |---|---|---|
//! | pending: in the plan, not adopted | yellow, the set then `…` | yellow `bluetooth…` |
//! | adopted | plain, the set | cyan `bluetooth` |
//! | no share, scan stopped | gray `idle` | gray `idle` |
//! | no share, scanning | gray `unassigned` | gray `unassigned` |

use ratatui::Frame;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use tui_scrollview::{ScrollView, ScrollViewState, ScrollbarVisibility};
use wartui_app::view::{FirmwareShown, NodeRow, NodeStatus, Severity, ShareShown, View};
use wartui_proto::bus::Board;
use wartui_proto::plan::ChannelSet;

use super::format::channel_set;
use crate::text::{channel_list, thousands};

/// The narrowest block: an inner 20 holds the longest states, `needs update` and
/// `rebooted xN`, and `channels   unassigned`.
const MIN_BLOCK_W: u16 = 22;

/// A block's height in the wide layout: five labeled lines and the border.
const WIDE_BLOCK_H: u16 = 7;

/// The wide block's `lost` line, there while any node has lost a batch.
const LOST_LINE_H: u16 = 1;

/// A block's height in the narrow layout: three bare lines and the border.
const NARROW_BLOCK_H: u16 = 5;

/// Where the blocks go: `cols` by `rows` blocks of `block_w` by `block_h`, across `width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grid {
    cols: usize,
    rows: usize,
    block_w: u16,
    block_h: u16,
    width: u16,
}

/// The grid for `n` blocks across `width` columns, wide blocks one line taller when `lost`.
///
/// Rows are balanced, so the last is never one lone block: as many rows as the widest fit
/// needs, then as few columns as fill them. The columns `width` leaves over go one each to the
/// first blocks of a row.
fn grid(n: usize, width: u16, wide: bool, lost: bool) -> Grid {
    let block_h = match (wide, lost) {
        (true, true) => WIDE_BLOCK_H + LOST_LINE_H,
        (true, false) => WIDE_BLOCK_H,
        (false, _) => NARROW_BLOCK_H,
    };
    let max_cols = usize::from(width / MIN_BLOCK_W).max(1);
    let rows = n.div_ceil(max_cols);
    let cols = if rows == 0 { 0 } else { n.div_ceil(rows) };
    // At most `max_cols`, which came from a `u16`.
    let block_w =
        u16::try_from(cols).map_or(width, |cols| width.checked_div(cols).unwrap_or(width));
    Grid { cols, rows, block_w, block_h, width }
}

impl Grid {
    /// The height of every row together.
    fn height(&self) -> u16 {
        u16::try_from(self.rows).unwrap_or(u16::MAX).saturating_mul(self.block_h)
    }

    /// Block `i`'s area, from the grid's top left.
    fn rect(&self, i: usize) -> Rect {
        let cols = self.cols.max(1);
        let (row, col) = (i / cols, i % cols);
        let col = u16::try_from(col).unwrap_or(u16::MAX);
        let used = self.block_w.saturating_mul(u16::try_from(cols).unwrap_or(u16::MAX));
        let extra = self.width.saturating_sub(used);
        let x = col.saturating_mul(self.block_w).saturating_add(col.min(extra));
        let y = u16::try_from(row).unwrap_or(u16::MAX).saturating_mul(self.block_h);
        Rect::new(x, y, self.block_w + u16::from(col < extra), self.block_h)
    }
}

/// Whether any node has lost a batch, which puts the `lost` line on every wide block. Like the
/// footer's `lost N`, it is left out until there is something to show.
fn any_lost(view: &View) -> bool {
    view.nodes.iter().any(|n| n.batches_lost > 0)
}

/// The rows the view's blocks across `width` want: every row of blocks and the detail line.
pub(super) fn wanted_height(view: &View, width: u16, wide: bool) -> u16 {
    let n = view.nodes.len();
    if n == 0 {
        return 0;
    }
    grid(n, width, wide, any_lost(view)).height().saturating_add(1)
}

/// Draw the node blocks into `area`, block `selected` under the cursor, and the selected node's
/// detail line under them when `area` has a row left after one row of blocks.
///
/// `scroll` is the offset from the last frame, moved only as far as keeps the selected row in
/// view: recomputed from the top, a move inside the window would scroll it. `cols` is set to the
/// blocks per row, which `↑` and `↓` move by.
pub(super) fn draw_blocks(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &View,
    selected: usize,
    scroll: &mut ScrollViewState,
    cols: &mut usize,
    wide: bool,
) {
    let n = view.nodes.len();
    let lost = any_lost(view);
    let full = grid(n, area.width, wide, lost);
    let (blocks, detail) = if area.height > full.block_h {
        let blocks = Rect { height: area.height - 1, ..area };
        (blocks, Some(Rect { y: area.bottom() - 1, height: 1, ..area }))
    } else {
        (area, None)
    };
    // A scrollbar takes the rightmost column, so a grid taller than its area is cut one
    // column narrower.
    let grid = if full.height() > blocks.height {
        self::grid(n, area.width.saturating_sub(1), wide, lost)
    } else {
        full
    };
    *cols = grid.cols.max(1);

    if let (Some(detail), Some(node)) = (detail, view.nodes.get(selected)) {
        let margin = Rect { x: detail.x + 1, width: detail.width.saturating_sub(1), ..detail };
        frame.render_widget(Paragraph::new(health_line(node)), margin);
    }
    // The scrollbar panics in an empty area.
    if n == 0 || blocks.width == 0 || blocks.height == 0 || grid.width == 0 {
        return;
    }

    let row = u16::try_from(selected / *cols).unwrap_or(u16::MAX);
    let top = row.saturating_mul(grid.block_h);
    let bottom = top.saturating_add(grid.block_h);
    let mut y = scroll.offset().y;
    if bottom > y.saturating_add(blocks.height) {
        y = bottom - blocks.height;
    }
    if top < y {
        y = top;
    }
    scroll.set_offset(Position::new(0, y));

    let mut scrolled = ScrollView::new(Size::new(grid.width, grid.height()))
        .horizontal_scrollbar_visibility(ScrollbarVisibility::Never)
        .vertical_scrollbar_visibility(ScrollbarVisibility::Automatic);
    for (i, node) in view.nodes.iter().enumerate() {
        let rect = grid.rect(i);
        let block = node_block(node, i == selected, Look { wide, lost }, rect.width);
        scrolled.render_widget(block, rect);
    }
    frame.render_stateful_widget(scrolled, blocks, scroll);
}

/// What every block on screen shares: the layout, and whether any node has lost a batch.
#[derive(Debug, Clone, Copy)]
struct Look {
    wide: bool,
    lost: bool,
}

/// One node's block, `width` columns wide, with the `lost` line when wide and `lost`.
fn node_block(node: &NodeRow, selected: bool, look: Look, width: u16) -> Paragraph<'static> {
    let border = border_style(node);
    let title_style = if selected { border.add_modifier(Modifier::BOLD) } else { border };
    let block = Block::bordered()
        .border_type(if selected { BorderType::Double } else { BorderType::Plain })
        .border_style(border)
        .title(Span::styled(format!(" {} ", slot_label(node)), title_style))
        .padding(Padding::left(1));
    // Two borders and the left padding, then one column clear of the right border.
    let text = usize::from(width.saturating_sub(4));
    let state = Line::from(Span::styled(status_word(node.status), severity_style(node.severity)));
    let lines = if look.wide {
        wide_lines(node, text, state, look)
    } else {
        narrow_lines(node, text, state)
    };
    Paragraph::new(lines).block(block)
}

/// The wide block's labeled lines: sightings, batches lost when any node lost one, die
/// temperature, channel count, channel list and state.
fn wide_lines(
    node: &NodeRow,
    width: usize,
    state: Line<'static>,
    look: Look,
) -> Vec<Line<'static>> {
    let warn = node.hot || node.supply_dropped;
    let mut reading = die_temp(node);
    if node.hot {
        reading.push_str(" hot");
    }
    if node.supply_dropped {
        reading.push_str(" supply drop");
    }
    let style = share_style(&node.share);
    let count = match &node.share {
        ShareShown::Channels { channels, pending } => {
            format!("{}{}", channels.len(), marker(*pending))
        }
        ShareShown::Bluetooth { pending } => format!("bluetooth{}", marker(*pending)),
        ShareShown::Idle => "idle".to_owned(),
        ShareShown::Unassigned => "unassigned".to_owned(),
    };
    let list = match &node.share {
        ShareShown::Channels { channels, .. } => channel_line(channel_set(channels), width, false),
        ShareShown::Bluetooth { .. } | ShareShown::Idle | ShareShown::Unassigned => String::new(),
    };
    let warning = severity_style(Severity::Warning);
    let mut lines = vec![Line::from(format!("sightings  {}", thousands(node.sightings)))];
    if look.lost {
        let style = if node.batches_lost > 0 { warning } else { Style::new() };
        lines.push(Line::from(vec![
            Span::raw("lost       "),
            Span::styled(thousands(node.batches_lost), style),
        ]));
    }
    lines.extend([
        Line::from(vec![
            Span::raw("die temp   "),
            Span::styled(reading, if warn { warning } else { Style::new() }),
        ]),
        Line::from(vec![Span::raw("channels   "), Span::styled(count, style)]),
        Line::from(Span::styled(list, style)),
        state,
    ]);
    lines
}

/// The narrow block's bare lines: sightings with the die temperature at the right, the share
/// with no count, and the state.
fn narrow_lines(node: &NodeRow, width: usize, state: Line<'static>) -> Vec<Line<'static>> {
    let sightings = format!("{} sightings", thousands(node.sightings));
    let temp = die_temp(node);
    let gap = width.saturating_sub(sightings.chars().count() + temp.chars().count()).max(1);
    let temp_style = if node.hot { severity_style(Severity::Warning) } else { Style::new() };
    vec![
        Line::from(vec![
            Span::raw(sightings),
            Span::raw(" ".repeat(gap)),
            Span::styled(temp, temp_style),
        ]),
        Line::from(share_span(&node.share, width)),
        state,
    ]
}

/// The node's latest die reading, `41 °C`, or `—` before one.
fn die_temp(node: &NodeRow) -> String {
    node.die_temp_c.map_or_else(|| "—".to_owned(), |c| format!("{c} °C"))
}

/// `…` while the node has yet to take its share.
fn marker(pending: bool) -> &'static str {
    if pending { "…" } else { "" }
}

/// The node's slot by its board's label, `N7`, or `—` with no slot.
fn slot_label(node: &NodeRow) -> &str {
    node.slot.as_ref().map_or("—", |slot| &slot.label)
}

/// The border's color: the state's when it asks anything of the operator, else blue for the
/// node whose assignment carries the Bluetooth scan and green for the rest.
fn border_style(node: &NodeRow) -> Style {
    match node.severity {
        Severity::Ok => {
            let ble = matches!(node.share, ShareShown::Bluetooth { .. });
            Style::new().fg(if ble { Color::Blue } else { Color::Green })
        }
        other => severity_style(other),
    }
}

/// A severity's color: green for `alive`, cyan for a notice (`rebooted xN`), yellow for a
/// warning, red for a fault.
const fn severity_style(severity: Severity) -> Style {
    Style::new().fg(match severity {
        Severity::Ok => Color::Green,
        Severity::Notice => Color::Cyan,
        Severity::Warning => Color::Yellow,
        Severity::Fault => Color::Red,
    })
}

/// The selected node's address, firmware and latest die reading, `—` for none, in the warning
/// style when hot: `N7 C5 57:84  v0.1.0  41 °C  lost 3`. `supply drop` follows, in the warning
/// style, while `NodeState::supply_dropped` holds, then `lost N`, in the warning style, once the
/// node has lost a batch. The rail reading itself never shows: its per-chip
/// offset makes the figure meaningless. A block has no width left for these, so they show for
/// one node at a time, and this is the one place the main screen shows a node's address.
fn health_line(node: &NodeRow) -> Line<'static> {
    let warning = severity_style(Severity::Warning);
    let mut spans = vec![
        Span::raw(format!(
            "{} {}  {}  ",
            node_label(node).trim_start(),
            node.mac_tail,
            version_label(node.firmware.as_ref())
        )),
        Span::styled(die_temp(node), if node.hot { warning } else { Style::new() }),
    ];
    if node.supply_dropped {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("supply drop", warning));
    }
    if node.batches_lost > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("lost {}", thousands(node.batches_lost)), warning));
    }
    Line::from(spans)
}

/// The style a share's lines take. The module docs list the states.
fn share_style(share: &ShareShown) -> Style {
    match share {
        ShareShown::Idle | ShareShown::Unassigned => Style::new().fg(Color::DarkGray),
        ShareShown::Channels { pending: true, .. } | ShareShown::Bluetooth { pending: true } => {
            Style::new().fg(Color::Yellow)
        }
        ShareShown::Bluetooth { pending: false } => Style::new().fg(Color::Cyan),
        ShareShown::Channels { pending: false, .. } => Style::new(),
    }
}

/// The narrow block's share line, `width` columns at most: the bare channel list, or
/// `bluetooth`, `idle` or `unassigned`, with `…` while it waits on the node. The Bluetooth node's
/// empty set would read `none` as a list, which looks like a fault rather than a job.
fn share_span(share: &ShareShown, width: usize) -> Span<'static> {
    let text = match share {
        ShareShown::Channels { channels, pending } => {
            channel_line(channel_set(channels), width, *pending)
        }
        ShareShown::Bluetooth { pending } => format!("bluetooth{}", marker(*pending)),
        ShareShown::Idle => "idle".to_owned(),
        ShareShown::Unassigned => "unassigned".to_owned(),
    };
    Span::styled(text, share_style(share))
}

/// The node's slot label and chip, `" N7 C5"`: the slot right-aligned to three columns, so `N1`,
/// `N14` and `—` (no slot) line up with the chip beside them.
fn node_label(node: &NodeRow) -> String {
    format!("{:>3} {}", slot_label(node), chip(node.board))
}

/// The node's chip, from the board its heartbeats announce. A node heard only through sightings
/// shows `—` until its first heartbeat, and a board this build has no name for shows `??`.
fn chip(board: Option<Board>) -> &'static str {
    match board {
        Some(Board::Esp32c5) => "C5",
        Some(Board::Esp32c6) => "C6",
        Some(Board::Esp32p4) => "P4",
        Some(Board::Unknown(_)) => "??",
        None => "—",
    }
}

/// The node's firmware, `v0.1.0`, with this build's after an arrow when the node's is older:
/// `v0.1.2 ↑v0.1.3`. `—` before a heartbeat.
fn version_label(firmware: Option<&FirmwareShown>) -> String {
    match firmware {
        Some(FirmwareShown { version, update_to: Some(here) }) => format!("v{version} ↑v{here}"),
        Some(FirmwareShown { version, update_to: None }) => format!("v{version}"),
        None => "—".to_owned(),
    }
}

/// Why a heartbeating node has no slot, the notice when a key that needs one is refused.
pub(super) const LOST_SLOT: &str =
    "lost its bus slot to another node's heartbeat, so it cannot be reached";

/// The state line: what the operator most needs to know about a node.
///
/// `stale`: sightings arrive, heartbeats do not. The bridge deals it no share. This
/// differs from silence, which has other causes. `off bus`: the bridge's `Status` shows the
/// node's slot absent, so the node lost power, crashed or was unplugged.
fn status_word(status: NodeStatus) -> String {
    match status {
        NodeStatus::NeedsUpdate => "needs update".to_owned(),
        NodeStatus::HostBehind => "host behind".to_owned(),
        NodeStatus::Starting => "starting".to_owned(),
        NodeStatus::NoHeartbeat => "no heartbeat".to_owned(),
        NodeStatus::OffBus => "off bus".to_owned(),
        NodeStatus::NoSlot => "no slot".to_owned(),
        NodeStatus::Stale => "stale".to_owned(),
        NodeStatus::Rebooted { times } => format!("rebooted x{times}"),
        NodeStatus::Alive => "alive".to_owned(),
    }
}

/// A channel set in at most `width` columns: as much of the list as fits, then `…` when it was
/// cut or, with `pending`, while the node has yet to take it.
///
/// The list is truncated rather than wrapped, because a round-robin share is a dozen scattered
/// channels. Either reason gives one `…`, never two: a round-robin share nearly always
/// overflows, so a second would be the usual case.
fn channel_line(set: ChannelSet, width: usize, pending: bool) -> String {
    let list = channel_list(set);
    // All ASCII, so bytes and columns match.
    if list.len() + usize::from(pending) <= width {
        return if pending { list + "…" } else { list };
    }
    // Cut at a comma. Cutting `1-11,36-165` by width alone can leave `1-1`, a range
    // that does not exist.
    let budget = width.saturating_sub(1);
    let mut kept = String::new();
    for group in list.split(',') {
        let with_group = if kept.is_empty() { group.len() } else { kept.len() + 1 + group.len() };
        if with_group > budget {
            break;
        }
        if !kept.is_empty() {
            kept.push(',');
        }
        kept.push_str(group);
    }
    kept + "…"
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;
    use wartui_app::settings::Settings;
    use wartui_core::engine::{NodeView, Snapshot};
    use wartui_proto::bus::{Capabilities, DieTemp, NODE_VERSION, NodeBoard, Slot, Vdd};
    use wartui_proto::fleet::ScanMode;
    use wartui_proto::plan::ChannelPool;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    /// `node`'s row, alone in [`busy`]: on a 14-node carrier, scanning Wi-Fi.
    fn row(node: NodeView) -> NodeRow {
        view(&Snapshot { nodes: vec![node], ..busy() }).nodes.remove(0)
    }

    /// The state line's word and the severity its color comes from, for `node`.
    fn node_state(node: &NodeView) -> (String, Severity) {
        let row = row(node.clone());
        (status_word(row.status), row.severity)
    }

    /// `snapshot` drawn at `width`×`height` through `ui`.
    fn terminal(
        snapshot: &Snapshot,
        ui: &mut Ui,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, ui)).expect("drawing");
        terminal
    }

    /// One row of `buffer`, cell by cell.
    fn cells(buffer: &Buffer, y: u16) -> Vec<&str> {
        (buffer.area.left()..buffer.area.right()).map(|x| buffer[(x, y)].symbol()).collect()
    }

    /// The block titled `title` (` N7 `): its top-left corner and its content rows' text.
    fn block(buffer: &Buffer, title: &str) -> Option<((u16, u16), Vec<String>)> {
        let corner = |c: &str| matches!(c, "┌" | "╔");
        let width = title.chars().count();
        (buffer.area.top()..buffer.area.bottom()).find_map(|y| {
            let row = cells(buffer, y);
            let at = row.windows(width).position(|w| w.concat() == title)?;
            let left = row[..at].iter().rposition(|&c| corner(c))?;
            let right = left + 1 + row[left + 1..].iter().position(|&c| matches!(c, "┐" | "╗"))?;
            let lines = (y + 1..buffer.area.bottom())
                .map(|y| cells(buffer, y))
                .take_while(|row| !matches!(row[left], "└" | "╚"))
                .map(|row| row[left + 1..right].concat())
                .collect();
            Some(((u16::try_from(left).ok()?, y), lines))
        })
    }

    /// The text of the block titled `title`, drawn from `snapshot` at 200×40.
    fn block_lines(snapshot: &Snapshot, title: &str) -> Vec<String> {
        let terminal = terminal(snapshot, &mut idle(), 200, 40);
        block(terminal.backend().buffer(), title).map(|(_, lines)| lines).unwrap_or_default()
    }

    /// The border color of the only node's block, N7.
    fn border_fg(node: NodeView) -> Option<Color> {
        let snapshot = Snapshot { nodes: vec![node], tail: Vec::new(), ..busy() };
        let terminal = terminal(&snapshot, &mut idle(), 200, 40);
        let buffer = terminal.backend().buffer();
        let ((x, y), _) = block(buffer, " N7 ")?;
        Some(buffer[(x, y)].fg)
    }

    /// The foreground of `label` in the only node's block, N7.
    fn state_fg(node: NodeView, label: &str) -> Option<Color> {
        let snapshot = Snapshot { nodes: vec![node], tail: Vec::new(), ..busy() };
        let terminal = terminal(&snapshot, &mut idle(), 200, 40);
        let buffer = terminal.backend().buffer();
        let ((left, top), lines) = block(buffer, " N7 ")?;
        let (dy, line) = lines.iter().enumerate().rfind(|(_, line)| line.contains(label))?;
        let dx = line
            .chars()
            .collect::<Vec<_>>()
            .windows(label.chars().count())
            .position(|w| w.iter().collect::<String>() == label)?;
        let x = left + 1 + u16::try_from(dx).ok()?;
        let y = top + 1 + u16::try_from(dy).ok()?;
        Some(buffer[(x, y)].fg)
    }

    #[test]
    fn blocks_wrap_five_per_row_when_fourteen_nodes_fit_120_columns() {
        let g = grid(14, 120, true, false);
        assert_eq!((g.cols, g.rows, g.block_w, g.block_h), (5, 3, 24, 7));
        let g = grid(1, 120, true, false);
        assert_eq!((g.cols, g.rows, g.block_w), (1, 1, 120));
    }

    #[test]
    fn blocks_balance_rows_when_last_row_would_hold_one_block() {
        // Five fit across, but five and one would leave a lone block.
        let g = grid(6, 120, true, false);
        assert_eq!((g.cols, g.rows, g.block_w), (3, 2, 40));
        let g = grid(6, 57, false, false);
        assert_eq!((g.cols, g.rows, g.block_h), (2, 3, 5));
    }

    #[test]
    fn blocks_spread_leftover_columns_when_width_does_not_divide() {
        let g = grid(2, 57, false, false);
        assert_eq!((g.rect(0), g.rect(1)), (Rect::new(0, 0, 29, 5), Rect::new(29, 0, 28, 5)));
        assert_eq!(g.rect(2), Rect::new(0, 5, 29, 5), "the next row starts at the left");
    }

    #[test]
    fn blocks_title_node_by_slot_when_rendering_nodes() {
        // `busy` is a carrier-14: byte 0x84 is A7, `N7`, and 0x86 is B2, `N9`.
        let mut slotless = node(0x88, 0, true);
        slotless.state.slot = None;
        let snapshot = Snapshot {
            nodes: vec![node(0x84, 0, true), unannounced(0x86), slotless],
            tail: Vec::new(),
            ..busy()
        };
        let terminal = terminal(&snapshot, &mut idle(), 200, 40);
        let buffer = terminal.backend().buffer();
        for title in [" N7 ", " N9 ", " — "] {
            let (_, lines) = block(buffer, title).unwrap_or_else(|| panic!("{title}"));
            assert!(lines.iter().all(|line| !line.contains("57:")), "no block shows an address");
        }
    }

    #[test]
    fn blocks_title_slot_by_silk_when_bridge_is_on_a_carrier() {
        // Byte 7 is B1, the first slot of bus B.
        let b1 = node(0x85, 0, true);
        assert_eq!(b1.state.slot, Slot::new(1, 0));
        let carrier = Snapshot { nodes: vec![b1.clone()], ..on_board(NodeBoard::Carrier14) };
        let bare = Snapshot { nodes: vec![b1], ..on_board(NodeBoard::NoCarrier) };
        assert!(!block_lines(&carrier, " N8 ").is_empty(), "{}", rendered(&carrier));
        // With no carrier the slot keeps its bus name.
        assert!(!block_lines(&bare, " B1 ").is_empty(), "{}", rendered(&bare));
    }

    #[test]
    fn blocks_distinguish_pending_from_adopted_shares_when_rendering_channels() {
        let screen = rendered(&busy());
        assert!(screen.contains("unassigned"), "a node with no share");
        assert!(screen.contains("channels   25…"), "in the plan, not yet adopted");
        assert!(screen.contains("36-165"), "and the channels in its share");
    }

    #[test]
    fn blocks_label_each_line_when_layout_is_wide() {
        let mut view = node(0x84, 0, true);
        view.state.die_temp = DieTemp::from_c(72);
        let lines = block_lines(&Snapshot { nodes: vec![view], ..busy() }, " N7 ");
        let lines: Vec<&str> = lines.iter().map(|line| line.trim_end()).collect();
        assert_eq!(
            lines,
            [" sightings  340", " die temp   72 °C hot", " channels   unassigned", "", " alive"]
        );
    }

    /// Fourteen nodes, N1 having lost three batches.
    fn lossy() -> Snapshot {
        let mut snapshot = fleet_of(14);
        snapshot.nodes[0].state.batches_lost = 3;
        snapshot
    }

    #[test]
    fn blocks_show_lost_line_on_every_wide_block_when_any_node_lost_batches() {
        let snapshot = lossy();
        let terminal = terminal(&snapshot, &mut idle(), 200, 40);
        let buffer = terminal.backend().buffer();
        // The count's cell: the left border, then ` lost       `.
        let count_fg = |((x, y), _): ((u16, u16), Vec<String>)| buffer[(x + 13, y + 2)].fg;
        for i in 1..=14 {
            let title = format!(" N{i} ");
            let found = block(buffer, &title).unwrap_or_else(|| panic!("{title}"));
            let lines = &found.1;
            assert_eq!(lines.len(), 6, "{title}: {lines:?}");
            let (want, fg) = if i == 1 { ("3", Color::Yellow) } else { ("0", Color::Reset) };
            assert_eq!(lines[1].trim_end(), format!(" lost       {want}"), "{title}");
            assert_eq!(count_fg(found), fg, "{title}: yellow only when nonzero");
        }
    }

    #[test]
    fn blocks_omit_lost_line_when_no_node_lost_batches() {
        let snapshot = fleet_of(14);
        assert_eq!(grid(14, 200, true, false).block_h, 7);
        let terminal = terminal(&snapshot, &mut idle(), 200, 40);
        let buffer = terminal.backend().buffer();
        for i in 1..=14 {
            let title = format!(" N{i} ");
            let (_, lines) = block(buffer, &title).unwrap_or_else(|| panic!("{title}"));
            assert_eq!(lines.len(), 5, "{title}: {lines:?}");
            assert!(lines.iter().all(|line| !line.contains("lost")), "{title}: {lines:?}");
        }
    }

    #[test]
    fn health_line_shows_lost_when_node_lost_batches() {
        let mut lossy = node(0x84, 0, true);
        lossy.state.batches_lost = 1_234;
        let line = health_line(&row(lossy));
        assert!(line.to_string().ends_with("  lost 1,234"), "{line}");
        let warning = line.spans.last().expect("the count");
        assert_eq!(warning.style.fg, Some(Color::Yellow));
        let line = health_line(&row(node(0x84, 0, true))).to_string();
        assert!(!line.contains("lost"), "{line}");
    }

    #[test]
    fn blocks_show_bare_channel_list_when_layout_is_narrow() {
        let snapshot = Snapshot { nodes: vec![pending(0x84), assigned(0x85)], ..busy() };
        let terminal = terminal(&snapshot, &mut idle(), 58, 26);
        let buffer = terminal.backend().buffer();
        let (_, lines) = block(buffer, " N7 ").expect("the pending node's block");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with(" 340 sightings"), "{lines:?}");
        assert_eq!(lines[1].trim(), "36-165…", "no count: {lines:?}");
        let (_, lines) = block(buffer, " N8 ").expect("the assigned node's block");
        assert_eq!(lines[1].trim(), "1", "{lines:?}");
    }

    /// A sparse node's share under the weighted deal on ten `us` nodes: 52, 60, 104, 116, 124,
    /// 132, 140 and 165, adopted.
    fn sparse_node(last: u8) -> NodeView {
        let mut view = node(last, 0, true);
        let mut channels = ChannelSet::empty();
        for channel in [52, 60, 104, 116, 124, 132, 140, 165] {
            channels.insert(wartui_proto::plan::index_of(channel).expect("a scan channel"));
        }
        give(&mut view, 3, channels);
        adopt(&mut view);
        view
    }

    #[test]
    fn blocks_show_eight_channel_count_when_node_holds_sparse_share() {
        let lines = block_lines(&Snapshot { nodes: vec![sparse_node(0x84)], ..busy() }, " N7 ");
        assert!(lines.iter().any(|l| l.trim_end() == " channels   8"), "{lines:?}");
    }

    #[test]
    fn blocks_fit_sparse_share_in_narrow_block_when_terminal_is_58_by_26() {
        // The handheld's narrow block has no count line; the list is cut at a comma to fit.
        let snapshot = Snapshot { nodes: vec![sparse_node(0x84), assigned(0x85)], ..busy() };
        let terminal = terminal(&snapshot, &mut idle(), 58, 26);
        let (_, lines) = block(terminal.backend().buffer(), " N7 ").expect("the sparse node");
        let list = lines[1].trim();
        assert!(list.starts_with("52,60,104,") && list.ends_with('…'), "{lines:?}");
        assert!(!list.contains(",…"), "never cut mid-separator: {lines:?}");
    }

    #[test]
    fn blocks_color_border_blue_when_node_scans_bluetooth() {
        let mut view = assigned(0x84);
        give(&mut view, 1, ChannelSet::empty());
        adopt(&mut view);
        assert_eq!(border_fg(view), Some(Color::Blue));
    }

    #[test]
    fn blocks_color_border_yellow_when_die_is_hot() {
        let mut hot = node(0x84, 0, true);
        hot.state.die_temp = DieTemp::from_c(70);
        assert_eq!(border_fg(hot), Some(Color::Yellow));
    }

    #[test]
    fn blocks_color_border_red_when_node_needs_update() {
        assert_eq!(border_fg(outdated(0x84)), Some(Color::Red));
    }

    #[test]
    fn blocks_color_border_cyan_when_node_rebooted() {
        assert_eq!(border_fg(node(0x84, 1, true)), Some(Color::Cyan));
    }

    #[test]
    fn blocks_color_border_green_when_node_is_alive() {
        assert_eq!(border_fg(node(0x84, 0, true)), Some(Color::Green));
    }

    #[test]
    fn blocks_draw_double_border_when_block_is_selected() {
        let snapshot = Snapshot { nodes: vec![node(0x84, 0, true), node(0x85, 0, true)], ..busy() };
        let mut ui = idle();
        let terminal = terminal(&snapshot, &mut ui, 200, 40);
        let buffer = terminal.backend().buffer();
        let ((x, y), _) = block(buffer, " N7 ").expect("the selected block");
        assert_eq!(buffer[(x, y)].symbol(), "╔");
        assert!(buffer[(x + 2, y)].modifier.contains(Modifier::BOLD), "a bold title");
        let ((x, y), _) = block(buffer, " N8 ").expect("the other block");
        assert_eq!(buffer[(x, y)].symbol(), "┌");
    }

    #[test]
    fn blocks_scroll_to_selected_row_when_grid_exceeds_area() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let press = |ui: &mut Ui, code| {
            ui.on_key(KeyEvent::new(code, KeyModifiers::NONE), &snapshot);
        };
        let mut ui = steering(&tx, Settings::default());
        // The first frame tells the keys how many blocks a row holds. At 32 rows the grid
        // shows two of its three rows, so a move between those two is inside the window.
        let mut terminal = terminal(&snapshot, &mut ui, 120, 32);
        for _ in 0..4 {
            press(&mut ui, KeyCode::Down);
            terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        }
        let screen = terminal.backend().to_string();
        assert!(screen.contains(" N14 "), "the last row scrolled into view: {screen}");
        assert!(!screen.contains(" N1 "), "the first row scrolled off: {screen}");

        // Moving the cursor up one row inside the window must not move the window. The
        // offset kept in `Ui` is what stops a recompute from row 0.
        let at_bottom = ui.grid_offset();
        press(&mut ui, KeyCode::Up);
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        assert_eq!(ui.grid_offset(), at_bottom, "the window moved for a cursor move inside it");
    }

    #[test]
    fn blocks_show_detail_line_for_selected_node() {
        let mut reading = node(0x84, 0, true);
        reading.state.vdd_baseline = Vdd::from_mv(3304);
        reading.state.vdd = Vdd::from_mv(3304);
        reading.state.die_temp = DieTemp::from_c(41);
        let silent = node(0x85, 0, true);
        let snapshot = Snapshot { nodes: vec![reading, silent], tail: Vec::new(), ..busy() };

        let mut ui = idle();
        let mut terminal = terminal(&snapshot, &mut ui, 200, 40);
        let screen = terminal.backend().to_string();
        assert!(screen.contains(&format!("N7 C5 57:84  v{NODE_VERSION}  41 °C")), "{screen}");

        ui.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &snapshot);
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains(&format!("N8 C5 57:85  v{NODE_VERSION}  —")),
            "no reading is a dash: {screen}"
        );
    }

    #[test]
    fn blocks_show_bluetooth_when_node_holds_or_awaits_ble() {
        let mut snapshot = busy();

        // Adopted: the node's heartbeat says it holds the Bluetooth share.
        give(&mut snapshot.nodes[0], 2, ChannelSet::empty());
        adopt(&mut snapshot.nodes[0]);
        snapshot.ble_slot = snapshot.nodes[0].state.slot;

        // In the plan, not yet adopted.
        give(&mut snapshot.nodes[3], 3, ChannelSet::empty());

        let adopted = block_lines(&snapshot, " N7 ");
        assert!(adopted.iter().any(|l| l.trim_end() == " channels   bluetooth"), "{adopted:?}");
        let waiting = block_lines(&snapshot, " N10 ");
        assert!(waiting.iter().any(|l| l.trim_end() == " channels   bluetooth…"), "{waiting:?}");
    }

    #[test]
    fn blocks_show_idle_when_scan_is_stopped_and_node_holds_no_share() {
        let snapshot = Snapshot {
            scan: Some(ScanMode::Stopped),
            nodes: vec![node(0x84, 0, true)],
            tail: Vec::new(),
            ..busy()
        };
        let lines = block_lines(&snapshot, " N7 ");
        assert!(lines.iter().any(|l| l.trim_end() == " channels   idle"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("unassigned")), "{lines:?}");
        let idle = view(&snapshot).nodes.remove(0).share;
        assert_eq!((share_style(&idle).fg, idle), (Some(Color::DarkGray), ShareShown::Idle));
        // A node the plan leaves out while the bridge scans has no share either, and says so.
        let scanning = Snapshot { scan: Some(ScanMode::Wifi), ..snapshot };
        let lines = block_lines(&scanning, " N7 ");
        assert!(lines.iter().any(|l| l.trim_end() == " channels   unassigned"), "{lines:?}");
    }

    #[test]
    fn blocks_show_idle_on_narrow_layout_when_scan_is_stopped() {
        let snapshot = Snapshot {
            scan: Some(ScanMode::Stopped),
            nodes: vec![node(0x84, 0, true), node(0x85, 0, true)],
            ..busy()
        };
        let terminal = terminal(&snapshot, &mut idle(), 58, 26);
        let (_, lines) = block(terminal.backend().buffer(), " N7 ").expect("N7's block");
        assert_eq!(lines[1].trim(), "idle", "{lines:?}");
    }

    #[test]
    fn blocks_color_pending_share_yellow_and_adopted_plain_when_node_takes_it() {
        let mut view = pending(0x84);
        assert_eq!(state_fg(view.clone(), "25…"), Some(Color::Yellow));
        adopt(&mut view);
        let lines = block_lines(&Snapshot { nodes: vec![view.clone()], ..busy() }, " N7 ");
        assert!(lines.iter().any(|l| l.trim_end() == " channels   25"), "{lines:?}");
        assert_eq!(state_fg(view, "25"), Some(Color::Reset));
    }

    #[test]
    fn channel_line_truncates_at_comma_when_width_is_limited() {
        let us = ChannelPool::Us.channels();
        assert_eq!(channel_line(us, 40, false), "1-11,36-165");
        assert_eq!(channel_line(us, 6, false), "1-11…", "and never cut mid-separator");
        assert_eq!(channel_line(us, 4, false), "…", "rather than an invented range like 1-1");
    }

    #[test]
    fn channel_line_emits_single_ellipsis_when_pending_list_is_truncated() {
        // The pending marker and the truncation marker are the same character. Two in
        // a row mean nothing more.
        let us = ChannelPool::Us.channels();
        assert_eq!(channel_line(us, 40, true), "1-11,36-165…");
        let mut comb = ChannelSet::empty();
        for idx in [0, 3, 6, 9, 12, 15, 18, 21] {
            comb.insert(idx);
        }
        let line = channel_line(comb, 12, true);
        assert!(!line.ends_with("……"), "one marker, not two: {line}");
        assert!(line.ends_with('…'), "still says it is pending: {line}");
        assert!(line.chars().count() <= 12, "and still fits: {line}");
    }

    #[test]
    fn share_span_renders_yellow_ellipsis_when_assignment_is_pending() {
        let span = share_span(&row(pending(0x11)).share, 20);
        assert_eq!(span.content, "36-165…");
        assert_eq!(span.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn share_span_renders_plain_when_assignment_is_adopted() {
        let view = assigned(0x11);
        assert!(view.adopted);
        let span = share_span(&row(view).share, 20);
        assert!(!span.content.ends_with('…'), "nothing left to wait for");
        assert_eq!(span.style.fg, None, "plain, not colored");
    }

    #[test]
    fn share_span_displays_bluetooth_when_node_is_assigned_bluetooth_scan() {
        // `none` would read as a fault rather than a job.
        let mut view = assigned(0x11);
        give(&mut view, 1, ChannelSet::empty());
        adopt(&mut view);
        assert_eq!(share_span(&row(view.clone()).share, 20).content, "bluetooth");

        view.adopted = false;
        assert_eq!(
            share_span(&row(view).share, 20).content,
            "bluetooth…",
            "and says so while it waits"
        );
    }

    #[test]
    fn node_state_returns_no_slot_label_when_another_node_took_its_slot() {
        let view = unslotted(0x21);
        let (label, _) = node_state(&view);
        assert_eq!(label, "no slot");
    }

    #[test]
    fn node_state_returns_off_bus_label_when_heartbeating_node_slot_is_absent() {
        let mut view = node(0x21, 0, false);
        view.alive = false;
        view.on_bus = false;
        assert_eq!(node_state(&view), ("off bus".to_owned(), Severity::Warning));
    }

    #[test]
    fn node_state_returns_no_heartbeat_label_when_node_is_only_heard_via_observations() {
        // Sightings have arrived but no heartbeat, so no window has opened to send
        // anything through.
        let view = unannounced(0x21);
        let (label, _) = node_state(&view);
        assert_eq!(label, "no heartbeat");
        assert!(view.state.observations > 0, "and it is not silent");
    }

    #[test]
    fn node_state_returns_starting_label_when_unannounced_node_is_inside_two_beats() {
        let mut view = unannounced(0x21);
        view.starting = true;
        assert_eq!(node_state(&view), ("starting".to_owned(), Severity::Ok));
    }

    #[test]
    fn node_state_returns_needs_update_label_when_node_runs_older_minor() {
        let view = outdated(0x84);
        let (label, severity) = node_state(&view);
        assert_eq!(label, "needs update");
        assert_eq!(severity, Severity::Fault);
    }

    #[test]
    fn node_state_returns_host_behind_label_when_node_runs_newer_minor() {
        let view = ahead(0x84);
        let (label, severity) = node_state(&view);
        assert_eq!(label, "host behind");
        assert_eq!(severity, Severity::Fault);
        assert_eq!(
            version_label(row(view.clone()).firmware.as_ref()),
            format!("v{}", view.state.capabilities.unwrap().version)
        );
    }

    #[test]
    fn health_line_shows_no_voltage_when_node_has_a_reading() {
        let mut steady = node(0x84, 0, true);
        steady.state.vdd_baseline = Vdd::from_mv(3304);
        steady.state.vdd = Vdd::from_mv(3304);
        let line = health_line(&row(steady)).to_string();
        assert!(!line.contains(" V"), "{line}");
        assert!(!line.contains("3.30"), "{line}");
        assert!(!line.contains("supply drop"), "{line}");
    }

    #[test]
    fn health_line_warns_of_supply_drop_when_reading_falls() {
        let mut dropped = node(0x84, 0, true);
        dropped.state.vdd_baseline = Vdd::from_mv(3304);
        dropped.state.vdd = Vdd::from_mv(3148);
        let line = health_line(&row(dropped));
        assert!(line.to_string().ends_with("  supply drop"), "{line}");
        assert!(!line.to_string().contains(" V"), "{line}");
        let warning = line.spans.last().expect("the warning");
        assert_eq!(
            (warning.content.as_ref(), warning.style.fg),
            ("supply drop", Some(Color::Yellow))
        );
    }

    #[test]
    fn blocks_warn_on_state_when_supply_drops() {
        let mut dropped = node(0x84, 0, true);
        dropped.state.vdd_baseline = Vdd::from_mv(3304);
        dropped.state.vdd = Vdd::from_mv(3148);
        assert_eq!(state_fg(dropped, "alive"), Some(Color::Yellow));
    }

    #[test]
    fn blocks_keep_state_style_when_reading_is_low_and_steady() {
        let mut low = node(0x84, 0, true);
        low.state.vdd_baseline = Vdd::from_mv(2920);
        low.state.vdd = Vdd::from_mv(2920);
        assert_eq!(state_fg(low, "alive"), Some(Color::Green));
    }

    #[test]
    fn blocks_warn_on_state_when_die_is_hot() {
        let mut hot = node(0x84, 0, true);
        hot.state.die_temp = DieTemp::from_c(70);
        assert_eq!(state_fg(hot.clone(), "alive"), Some(Color::Yellow));
        assert_eq!(state_fg(hot, "70 °C hot"), Some(Color::Yellow), "and the reading");
    }

    #[test]
    fn blocks_keep_state_style_when_readings_are_in_range() {
        let mut normal = node(0x84, 0, true);
        // One step short of a drop.
        normal.state.vdd_baseline = Vdd::from_mv(3304);
        normal.state.vdd = Vdd::from_mv(3160);
        normal.state.die_temp = DieTemp::from_c(69);
        assert_eq!(state_fg(normal, "alive"), Some(Color::Green));
    }

    #[test]
    fn blocks_keep_notice_style_when_rebooted_node_has_supply_drop() {
        // The reboot is the more useful news; the detail line still shows the drop.
        let mut rebooted = node(0x84, 1, true);
        rebooted.state.vdd_baseline = Vdd::from_mv(3304);
        rebooted.state.vdd = Vdd::from_mv(3148);
        assert_eq!(state_fg(rebooted, "rebooted x1"), Some(Color::Cyan));
    }

    #[test]
    fn node_label_names_chip_from_board_when_node_has_heartbeated() {
        assert_eq!(node_label(&row(node(0x84, 0, true))), " N7 C5");
        let mut c6 = node(0x84, 0, true);
        c6.state.capabilities = Some(Capabilities::here(Board::Esp32c6));
        assert_eq!(node_label(&row(c6)), " N7 C6");
        let mut unknown = node(0x84, 0, true);
        unknown.state.capabilities = Some(Capabilities::here(Board::Unknown(0x40)));
        assert_eq!(node_label(&row(unknown)), " N7 ??");
        assert_eq!(node_label(&row(unannounced(0x84))), " N7 —");
    }

    #[test]
    fn node_label_aligns_slot_when_slot_is_two_digits_or_none() {
        // Byte 0x8B is B7, `N14`.
        assert_eq!(node_label(&row(node(0x8B, 0, true))), "N14 C5");
        assert_eq!(node_label(&row(unslotted(0x84))), "  — C5");
    }

    #[test]
    fn version_label_shows_arrow_when_node_patch_is_behind() {
        // Which version is behind is `wartui_app::view`'s rule, tested there against an explicit
        // build version. These are the words.
        let shown = |update_to: Option<&str>| FirmwareShown {
            version: "0.1.2".to_owned(),
            update_to: update_to.map(str::to_owned),
        };
        assert_eq!(version_label(Some(&shown(Some("0.1.3")))), "v0.1.2 ↑v0.1.3");
        assert_eq!(version_label(Some(&shown(None))), "v0.1.2", "current: no arrow");
        assert_eq!(version_label(row(unannounced(0x84)).firmware.as_ref()), "—");
    }

    #[test]
    fn version_label_shows_version_alone_when_node_is_on_trial() {
        let mut trial = node(0x84, 0, true);
        let version = NODE_VERSION;
        let caps = Capabilities::here(Board::Esp32c5);
        trial.state.capabilities =
            Some(Capabilities { version, trial: true, five_ghz: true, ..caps });
        assert_eq!(version_label(row(trial).firmware.as_ref()), format!("v{version}"));
    }

    #[test]
    fn blocks_show_host_behind_when_node_runs_newer_minor() {
        let view = ahead(0x84);
        let caps = view.state.capabilities.expect("set");
        let snapshot = Snapshot { nodes: vec![view], tail: Vec::new(), ..busy() };
        let screen = rendered(&snapshot);
        assert!(screen.contains("host behind"), "{screen}");
        assert!(!screen.contains("needs update"), "{screen}");
        // The detail line: the node's version alone, no arrow toward this build's.
        assert!(screen.contains(&format!("N7 C5 57:84  v{}  ", caps.version)), "{screen}");
    }

    #[test]
    fn blocks_show_needs_update_and_version_when_node_runs_older_minor() {
        let snapshot = Snapshot { nodes: vec![outdated(0x84)], tail: Vec::new(), ..busy() };
        let lines = block_lines(&snapshot, " N7 ");
        assert_eq!(lines.last().map(|l| l.trim()), Some("needs update"), "{lines:?}");
        let older = outdated(0x84).state.capabilities.expect("set").version;
        let screen = rendered(&snapshot);
        assert!(screen.contains(&format!("v{older} ↑v{NODE_VERSION}")), "{screen}");
    }
}
