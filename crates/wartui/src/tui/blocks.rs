//! One block per node, across the width of the screen, because the figures an operator checks
//! most are per node and a laptop has the columns for all fourteen at once. Blocks never stretch
//! vertically: a screen too short for every row scrolls them, and the strip above takes the rest.
//!
//! A block's border carries its state's color, and its last line still spells the state out,
//! because color alone is not the signal: a terminal without color, or an operator who cannot
//! tell red from green, reads the word.
//!
//! The channel lines color each node's share by how far its assignment has got, because a
//! delivery is not adoption. Only the node's heartbeat, carrying the epoch it holds, shows it
//! took the frame (`NodeState::adopted`).
//!
//! | State | Wi-Fi share | Bluetooth node |
//! |---|---|---|
//! | pending: sent, not acked | yellow, the desired set then `…` | yellow `bluetooth…` |
//! | acked, not adopted | blue, the confirmed set then `…` | blue `bluetooth…` |
//! | adopted | plain, the confirmed set | cyan `bluetooth` |
//! | nothing confirmed | gray `unassigned` | gray `unassigned` |

use ratatui::Frame;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use tui_scrollview::{ScrollView, ScrollViewState, ScrollbarVisibility};
use wartui_core::engine::{NodeView, Snapshot};
use wartui_core::record::AdminOutcome;
use wartui_proto::bus::{Board, Compat, NODE_VERSION, NodeBoard, Version};
use wartui_proto::mac;
use wartui_proto::plan::{ChannelSet, SCAN_CHANNELS};

use crate::text::thousands;

/// The narrowest block: an inner 20 holds the longest state, `no admin ack`, and
/// `channels   unassigned`.
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
fn any_lost(snapshot: &Snapshot) -> bool {
    snapshot.nodes.iter().any(|n| n.state.batches_lost > 0)
}

/// The rows the snapshot's blocks across `width` want: every row of blocks and the detail line.
pub(super) fn wanted_height(snapshot: &Snapshot, width: u16, wide: bool) -> u16 {
    let n = snapshot.nodes.len();
    if n == 0 {
        return 0;
    }
    grid(n, width, wide, any_lost(snapshot)).height().saturating_add(1)
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
    snapshot: &Snapshot,
    selected: usize,
    scroll: &mut ScrollViewState,
    cols: &mut usize,
    wide: bool,
) {
    let n = snapshot.nodes.len();
    let lost = any_lost(snapshot);
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

    let board = snapshot.node_board();
    if let (Some(detail), Some(node)) = (detail, snapshot.nodes.get(selected)) {
        let margin = Rect { x: detail.x + 1, width: detail.width.saturating_sub(1), ..detail };
        frame.render_widget(Paragraph::new(health_line(board, node)), margin);
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

    let mut view = ScrollView::new(Size::new(grid.width, grid.height()))
        .horizontal_scrollbar_visibility(ScrollbarVisibility::Never)
        .vertical_scrollbar_visibility(ScrollbarVisibility::Automatic);
    for (i, node) in snapshot.nodes.iter().enumerate() {
        let rect = grid.rect(i);
        view.render_widget(node_block(board, node, i == selected, wide, lost, rect.width), rect);
    }
    frame.render_stateful_widget(view, blocks, scroll);
}

/// One node's block, `width` columns wide, with the `lost` line when wide and `lost`.
fn node_block(
    board: NodeBoard,
    node: &NodeView,
    selected: bool,
    wide: bool,
    lost: bool,
    width: u16,
) -> Paragraph<'static> {
    let (state, severity) = raised_state(node);
    let border = border_style(node, severity);
    let title_style = if selected { border.add_modifier(Modifier::BOLD) } else { border };
    let block = Block::bordered()
        .border_type(if selected { BorderType::Double } else { BorderType::Plain })
        .border_style(border)
        .title(Span::styled(format!(" {} ", slot_label(board, node)), title_style))
        .padding(Padding::left(1));
    // Two borders and the left padding, then one column clear of the right border.
    let text = usize::from(width.saturating_sub(4));
    let state = Line::from(Span::styled(state, severity.style()));
    let lines =
        if wide { wide_lines(node, text, state, lost) } else { narrow_lines(node, text, state) };
    Paragraph::new(lines).block(block)
}

/// The wide block's labeled lines: sightings, batches lost when `lost`, die temperature, channel
/// count, channel list and state.
fn wide_lines(
    node: &NodeView,
    width: usize,
    state: Line<'static>,
    lost: bool,
) -> Vec<Line<'static>> {
    let s = &node.state;
    let warn = s.hot() || s.supply_dropped();
    let mut reading = s.die_temp.to_string();
    if s.hot() {
        reading.push_str(" hot");
    }
    if s.supply_dropped() {
        reading.push_str(" supply drop");
    }
    let (share, waiting, style) = share(node);
    let marker = if waiting { "…" } else { "" };
    let count = match share {
        Share::Channels(set) => format!("{}{marker}", set.len()),
        Share::Bluetooth => format!("bluetooth{marker}"),
        Share::Unassigned => "unassigned".to_owned(),
    };
    let list = match share {
        Share::Channels(set) => channel_line(set, width, false),
        Share::Bluetooth | Share::Unassigned => String::new(),
    };
    let mut lines = vec![Line::from(format!("sightings  {}", thousands(s.observations)))];
    if lost {
        let style = if s.batches_lost > 0 { Severity::Warning.style() } else { Style::new() };
        lines.push(Line::from(vec![
            Span::raw("lost       "),
            Span::styled(thousands(s.batches_lost), style),
        ]));
    }
    lines.extend([
        Line::from(vec![
            Span::raw("die temp   "),
            Span::styled(reading, if warn { Severity::Warning.style() } else { Style::new() }),
        ]),
        Line::from(vec![Span::raw("channels   "), Span::styled(count, style)]),
        Line::from(Span::styled(list, style)),
        state,
    ]);
    lines
}

/// The narrow block's bare lines: sightings with the die temperature at the right, the share
/// with no count, and the state.
fn narrow_lines(node: &NodeView, width: usize, state: Line<'static>) -> Vec<Line<'static>> {
    let s = &node.state;
    let sightings = format!("{} sightings", thousands(s.observations));
    let temp = s.die_temp.to_string();
    let gap = width.saturating_sub(sightings.chars().count() + temp.chars().count()).max(1);
    let temp_style = if s.hot() { Severity::Warning.style() } else { Style::new() };
    vec![
        Line::from(vec![
            Span::raw(sightings),
            Span::raw(" ".repeat(gap)),
            Span::styled(temp, temp_style),
        ]),
        Line::from(share_span(node, width)),
        state,
    ]
}

/// The node's slot by its board's label, `N7`, or `—` with no slot.
fn slot_label(board: NodeBoard, node: &NodeView) -> String {
    node.state.slot.map_or_else(|| "—".to_owned(), |slot| board.label(slot).to_string())
}

/// The node's state, raised to a warning when an otherwise healthy node has a supply drop or a
/// hot die: any state of its own says more.
fn raised_state(node: &NodeView) -> (String, Severity) {
    let (state, mut severity) = node_state(node);
    if severity == Severity::Ok && (node.state.supply_dropped() || node.state.hot()) {
        severity = Severity::Warning;
    }
    (state, severity)
}

/// The border's color: the state's when it asks anything of the operator, else blue for the
/// node whose assignment carries the Bluetooth scan and green for the rest.
fn border_style(node: &NodeView, severity: Severity) -> Style {
    match severity {
        Severity::Ok => {
            let ble = node.state.desired.or(node.state.confirmed).is_some_and(|a| a.ble);
            Style::new().fg(if ble { Color::Blue } else { Color::Green })
        }
        other => other.style(),
    }
}

/// How much a node's state asks of the operator, which decides its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    /// `alive`: nothing to do.
    Ok,
    /// Worth knowing, needing nothing: `rebooted xN`.
    Notice,
    /// Out of the plan for now, a supply drop or a hot die.
    Warning,
    /// Something to fix.
    Fault,
}

impl Severity {
    const fn style(self) -> Style {
        Style::new().fg(match self {
            Self::Ok => Color::Green,
            Self::Notice => Color::Cyan,
            Self::Warning => Color::Yellow,
            Self::Fault => Color::Red,
        })
    }
}

/// The selected node's address, firmware and latest die reading, `—` for none, in the warning
/// style when hot: `N7 C5 57:84  v0.1.0  41 °C  lost 3`. `supply drop` follows, in the warning
/// style, while `NodeState::supply_dropped` holds, then `lost N`, in the warning style, once the
/// node has lost a batch. The rail reading itself never shows: its per-chip
/// offset makes the figure meaningless. A block has no width left for these, so they show for
/// one node at a time, and this is the one place the main screen shows a node's address.
fn health_line(board: NodeBoard, node: &NodeView) -> Line<'static> {
    let state = &node.state;
    let temp = state.die_temp.to_string();
    let mut spans = vec![
        Span::raw(format!(
            "{} {}  {}  ",
            node_label(board, node).trim_start(),
            mac::short(&state.mac),
            version_label(node, NODE_VERSION)
        )),
        Span::styled(temp, if state.hot() { Severity::Warning.style() } else { Style::new() }),
    ];
    if state.supply_dropped() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("supply drop", Severity::Warning.style()));
    }
    if state.batches_lost > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("lost {}", thousands(state.batches_lost)),
            Severity::Warning.style(),
        ));
    }
    Line::from(spans)
}

/// What a node scans, as the channel lines show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Share {
    Channels(ChannelSet),
    /// An empty set, the Bluetooth node's. As a list it would read `none`, which looks like a
    /// fault rather than a job.
    Bluetooth,
    Unassigned,
}

/// The node's share, whether it still waits on the node, and the style its lines take. The
/// module docs list the states.
fn share(node: &NodeView) -> (Share, bool, Style) {
    let state = &node.state;
    let of = |set: ChannelSet| if set.is_empty() { Share::Bluetooth } else { Share::Channels(set) };
    if state.dirty
        && let Some(desired) = state.desired
    {
        return (of(desired.channels), true, Style::new().fg(Color::Yellow));
    }
    let Some(confirmed) = state.confirmed else {
        return (Share::Unassigned, false, Style::new().fg(Color::DarkGray));
    };
    let share = of(confirmed.channels);
    match (share, state.adopted()) {
        (Share::Bluetooth, true) => (share, false, Style::new().fg(Color::Cyan)),
        (_, true) => (share, false, Style::new()),
        (_, false) => (share, true, Style::new().fg(Color::Blue)),
    }
}

/// The narrow block's share line, `width` columns at most: the bare channel list, or
/// `bluetooth` or `unassigned`, with `…` while it waits on the node.
fn share_span(node: &NodeView, width: usize) -> Span<'static> {
    let (share, waiting, style) = share(node);
    let text = match share {
        Share::Channels(set) => channel_line(set, width, waiting),
        Share::Bluetooth if waiting => "bluetooth…".to_owned(),
        Share::Bluetooth => "bluetooth".to_owned(),
        Share::Unassigned => "unassigned".to_owned(),
    };
    Span::styled(text, style)
}

/// The node's slot label and chip, `" N7 C5"`: the slot right-aligned to three columns, so `N1`,
/// `N14` and `—` (no slot) line up with the chip beside them.
fn node_label(board: NodeBoard, node: &NodeView) -> String {
    format!("{:>3} {}", slot_label(board, node), chip(node))
}

/// The node's chip, from the board its heartbeats announce. A node heard only through sightings
/// shows `—` until its first heartbeat, and a board this build has no name for shows `??`.
fn chip(node: &NodeView) -> &'static str {
    match node.state.capabilities.map(|caps| caps.board) {
        Some(Board::Esp32c5) => "C5",
        Some(Board::Esp32c6) => "C6",
        Some(Board::Esp32p4) => "P4",
        Some(Board::Unknown(_)) => "??",
        None => "—",
    }
}

/// The node's firmware, `v0.1.0`, with `here` (the version this host was built with) after an
/// arrow when the node's is older: `v0.1.2 ↑v0.1.3`. `—` before a heartbeat.
fn version_label(node: &NodeView, here: Version) -> String {
    match node.state.capabilities {
        Some(caps) if here.compat(&caps.version).node_behind() => {
            format!("v{} ↑v{here}", caps.version)
        }
        Some(caps) => format!("v{}", caps.version),
        None => "—".to_owned(),
    }
}

/// How the node's firmware stands against this build's, once a heartbeat has said.
fn compat(node: &NodeView) -> Option<Compat> {
    node.state.capabilities.map(|caps| caps.version.compat_with_host())
}

/// Why a node cannot be given an assignment, or `None` if it can.
///
/// The text is the notice shown when `b` is refused, so it says what is wrong
/// rather than naming a state.
pub(super) fn why_not_assignable(node: &NodeView) -> Option<&'static str> {
    let state = &node.state;
    if state.capabilities.is_none() {
        return Some("has not heartbeated yet, so nothing says what its radio can tune");
    }
    match compat(node) {
        Some(Compat::NodeBehind) => {
            return Some("runs older firmware than this host drives, so it needs an update first");
        }
        Some(Compat::NodeAhead) => {
            return Some(
                "runs newer firmware than this host drives, so update the host to drive it",
            );
        }
        _ => {}
    }
    if state.slot.is_none() {
        return Some(LOST_SLOT);
    }
    if !node.on_bus {
        return Some("is off the node bus, so it cannot be reached");
    }
    if !node.assignable {
        return Some("is not heartbeating, so it cannot be assigned");
    }
    None
}

/// Why a heartbeating node has no slot, the notice when a key that needs one is refused.
pub(super) const LOST_SLOT: &str =
    "lost its bus slot to another node's heartbeat, so it cannot be reached";

/// The state line: what the operator most needs to know about a node.
///
/// `stale`: sightings arrive, heartbeats do not. The node cannot be assigned. This
/// differs from silence, which has other causes. `off bus`: the bridge's `Status` shows the
/// node's slot absent, so the node lost power, crashed or was unplugged.
fn node_state(node: &NodeView) -> (String, Severity) {
    let state = &node.state;
    // Ahead of `no heartbeat`: a node on another major or minor has its heartbeats read
    // only as far as its version, and they never count as liveness.
    match compat(node) {
        Some(Compat::NodeBehind) => return ("needs update".to_owned(), Severity::Fault),
        Some(Compat::NodeAhead) => return ("host behind".to_owned(), Severity::Fault),
        _ => {}
    }
    if state.last_heartbeat.is_none() {
        return ("no heartbeat".to_owned(), Severity::Warning);
    }
    if state.slot.is_some() && !node.on_bus {
        return ("off bus".to_owned(), Severity::Warning);
    }
    // Ahead of `stale`. A node whose slot another node's heartbeat took is unreachable
    // until its own heartbeat names a slot again.
    if state.slot.is_none() {
        return ("no slot".to_owned(), Severity::Warning);
    }
    if !node.assignable {
        return ("stale".to_owned(), Severity::Warning);
    }
    // The most actionable state: the bridge could not deliver the node's assignment, or
    // never answered for it.
    if matches!(state.last_outcome, Some(AdminOutcome::Unacked | AdminOutcome::Silent)) {
        return ("no admin ack".to_owned(), Severity::Fault);
    }
    if state.reboots > 0 {
        return (format!("rebooted x{}", state.reboots), Severity::Notice);
    }
    ("alive".to_owned(), Severity::Ok)
}

/// Format a set as channel numbers, the units every other tool uses.
///
/// Runs collapse to `first-last`, so a contiguous set reads `1-11`.
fn channel_list(set: ChannelSet) -> String {
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
    use wartui_core::engine::Assignment;
    use wartui_proto::bus::{Capabilities, DieTemp, NodeBoard, Slot, Vdd};
    use wartui_proto::plan::{ChannelPool, IndexRun};

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

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
        let terminal = terminal(snapshot, &mut Ui::default(), 200, 40);
        block(terminal.backend().buffer(), title).map(|(_, lines)| lines).unwrap_or_default()
    }

    /// The border color of the only node's block, N7.
    fn border_fg(node: NodeView) -> Option<Color> {
        let snapshot = Snapshot { nodes: vec![node], tail: Vec::new(), ..busy() };
        let terminal = terminal(&snapshot, &mut Ui::default(), 200, 40);
        let buffer = terminal.backend().buffer();
        let ((x, y), _) = block(buffer, " N7 ")?;
        Some(buffer[(x, y)].fg)
    }

    /// The foreground of `label` in the only node's block, N7.
    fn state_fg(node: NodeView, label: &str) -> Option<Color> {
        let snapshot = Snapshot { nodes: vec![node], tail: Vec::new(), ..busy() };
        let terminal = terminal(&snapshot, &mut Ui::default(), 200, 40);
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
        let terminal = terminal(&snapshot, &mut Ui::default(), 200, 40);
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
    fn blocks_distinguish_confirmed_from_desired_assignments_when_rendering_channels() {
        let screen = rendered(&busy());
        assert!(screen.contains("unassigned"), "a node nobody has assigned");
        assert!(screen.contains("channels   25…"), "asked for, not yet acknowledged");
        assert!(screen.contains("36-165"), "and the channels asked for");
        assert!(screen.contains("no admin ack"), "sent, and the node never answered");
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
        let terminal = terminal(&snapshot, &mut Ui::default(), 200, 40);
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
        let terminal = terminal(&snapshot, &mut Ui::default(), 200, 40);
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
        let line = health_line(NodeBoard::Carrier14, &lossy);
        assert!(line.to_string().ends_with("  lost 1,234"), "{line}");
        let warning = line.spans.last().expect("the count");
        assert_eq!(warning.style.fg, Some(Color::Yellow));
        let line = health_line(NodeBoard::Carrier14, &node(0x84, 0, true)).to_string();
        assert!(!line.contains("lost"), "{line}");
    }

    #[test]
    fn blocks_show_bare_channel_list_when_layout_is_narrow() {
        let snapshot = Snapshot { nodes: vec![pending(0x84), assigned(0x85)], ..busy() };
        let terminal = terminal(&snapshot, &mut Ui::default(), 58, 26);
        let buffer = terminal.backend().buffer();
        let (_, lines) = block(buffer, " N7 ").expect("the pending node's block");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with(" 340 sightings"), "{lines:?}");
        assert_eq!(lines[1].trim(), "36-165…", "no count: {lines:?}");
        let (_, lines) = block(buffer, " N8 ").expect("the assigned node's block");
        assert_eq!(lines[1].trim(), "1", "{lines:?}");
    }

    #[test]
    fn blocks_color_border_blue_when_node_scans_bluetooth() {
        let mut view = assigned(0x84);
        let ble = Assignment { channels: ChannelSet::empty(), ble: true, counter: 1 };
        view.state.confirmed = Some(ble);
        view.state.desired = Some(ble);
        adopt(&mut view.state);
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
        let mut ui = Ui::default();
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
            ui.on_key(KeyEvent::new(code, KeyModifiers::NONE), &snapshot, &tx);
        };
        let mut ui = Ui::default();
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

        let mut ui = Ui::default();
        let mut terminal = terminal(&snapshot, &mut ui, 200, 40);
        let screen = terminal.backend().to_string();
        assert!(screen.contains(&format!("N7 C5 57:84  v{NODE_VERSION}  41 °C")), "{screen}");

        let (tx, _rx) = mpsc::channel(4);
        ui.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &snapshot, &tx);
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
        let mac = snapshot.nodes[0].state.mac;

        // Adopted: confirmed and desired agree on the Bluetooth assignment, and the
        // node's heartbeat says it holds it.
        let existing = snapshot.nodes[0].state.confirmed.expect("assigned() sets one");
        let ble = Assignment { channels: ChannelSet::empty(), ble: true, ..existing };
        snapshot.nodes[0].state.confirmed = Some(ble);
        snapshot.nodes[0].state.desired = Some(ble);
        adopt(&mut snapshot.nodes[0].state);
        snapshot.ble_node = Some(mac);

        // Still dirty: sent but not yet acknowledged.
        if let Some(desired) = snapshot.nodes[3].state.desired.as_mut() {
            desired.channels = ChannelSet::empty();
            desired.ble = true;
        }

        let adopted = block_lines(&snapshot, " N7 ");
        assert!(adopted.iter().any(|l| l.trim_end() == " channels   bluetooth"), "{adopted:?}");
        let waiting = block_lines(&snapshot, " N10 ");
        assert!(waiting.iter().any(|l| l.trim_end() == " channels   bluetooth…"), "{waiting:?}");
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
        let span = share_span(&pending(0x11), 20);
        assert_eq!(span.content, "36-165…");
        assert_eq!(span.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn share_span_renders_blue_ellipsis_when_acknowledged_but_not_adopted() {
        let view = acked_not_adopted(0x11);
        assert!(!view.state.adopted());
        let span = share_span(&view, 20);
        assert!(span.content.ends_with('…'), "the confirmed set, marked as not yet adopted");
        assert!(span.content.starts_with('1'), "the confirmed set is still shown");
        assert_eq!(span.style.fg, Some(Color::Blue));
    }

    #[test]
    fn share_span_renders_plain_when_assignment_is_adopted() {
        let view = assigned(0x11);
        assert!(view.state.adopted());
        let span = share_span(&view, 20);
        assert!(!span.content.ends_with('…'), "nothing left to wait for");
        assert_eq!(span.style.fg, None, "plain, not colored");
    }

    #[test]
    fn share_span_renders_plain_when_desired_is_cleared_and_confirmed_is_held() {
        // A node that leaves the plan (`replan`'s departed loop, or `PeerTableFull`)
        // keeps `confirmed` while `desired` clears. `adopted` must check what it
        // still holds.
        let mut view = assigned(0x11);
        view.state.desired = None;
        assert!(view.state.adopted(), "confirmed is still held, and its epoch still matches");
        let span = share_span(&view, 20);
        assert!(!span.content.ends_with('…'), "adopted, not stuck waiting");
        assert_eq!(span.style.fg, None, "plain, not colored");
    }

    #[test]
    fn share_span_displays_bluetooth_when_node_is_assigned_bluetooth_scan() {
        // `none` would read as a fault rather than a job.
        let mut view = assigned(0x11);
        let assignment = Assignment { channels: ChannelSet::empty(), ble: true, counter: 1 };
        view.state.confirmed = Some(assignment);
        view.state.desired = Some(assignment);
        adopt(&mut view.state);
        assert_eq!(share_span(&view, 20).content, "bluetooth");

        view.state.dirty = true;
        assert_eq!(share_span(&view, 20).content, "bluetooth…", "and says so while it waits");
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
        let why = why_not_assignable(&view).expect("refused");
        assert!(why.contains("off the node bus"), "{why}");
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
    fn node_state_returns_needs_update_label_when_node_runs_older_minor() {
        let view = outdated(0x84);
        let (label, severity) = node_state(&view);
        assert_eq!(label, "needs update");
        assert_eq!(severity, Severity::Fault);
        assert!(why_not_assignable(&view).is_some_and(|why| why.contains("update")));
    }

    #[test]
    fn node_state_returns_host_behind_label_when_node_runs_newer_minor() {
        let view = ahead(0x84);
        let (label, severity) = node_state(&view);
        assert_eq!(label, "host behind");
        assert_eq!(severity, Severity::Fault);
        let why = why_not_assignable(&view).expect("refused");
        assert!(why.contains("update the host"), "{why}");
        assert_eq!(
            version_label(&view, NODE_VERSION),
            format!("v{}", view.state.capabilities.unwrap().version)
        );
    }

    #[test]
    fn health_line_shows_no_voltage_when_node_has_a_reading() {
        let mut steady = node(0x84, 0, true);
        steady.state.vdd_baseline = Vdd::from_mv(3304);
        steady.state.vdd = Vdd::from_mv(3304);
        let line = health_line(NodeBoard::Carrier14, &steady).to_string();
        assert!(!line.contains(" V"), "{line}");
        assert!(!line.contains("3.30"), "{line}");
        assert!(!line.contains("supply drop"), "{line}");
    }

    #[test]
    fn health_line_warns_of_supply_drop_when_reading_falls() {
        let mut dropped = node(0x84, 0, true);
        dropped.state.vdd_baseline = Vdd::from_mv(3304);
        dropped.state.vdd = Vdd::from_mv(3148);
        let line = health_line(NodeBoard::Carrier14, &dropped);
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
        let board = NodeBoard::Carrier14;
        assert_eq!(node_label(board, &node(0x84, 0, true)), " N7 C5");
        let mut c6 = node(0x84, 0, true);
        c6.state.capabilities = Some(Capabilities::here(Board::Esp32c6));
        assert_eq!(node_label(board, &c6), " N7 C6");
        let mut unknown = node(0x84, 0, true);
        unknown.state.capabilities = Some(Capabilities::here(Board::Unknown(0x40)));
        assert_eq!(node_label(board, &unknown), " N7 ??");
        assert_eq!(node_label(board, &unannounced(0x84)), " N7 —");
    }

    #[test]
    fn node_label_aligns_slot_when_slot_is_two_digits_or_none() {
        let board = NodeBoard::Carrier14;
        // Byte 0x8B is B7, `N14`.
        assert_eq!(node_label(board, &node(0x8B, 0, true)), "N14 C5");
        assert_eq!(node_label(board, &unslotted(0x84)), "  — C5");
    }

    #[test]
    fn version_label_shows_arrow_when_node_patch_is_behind() {
        let mut behind = node(0x84, 0, true);
        let caps = Capabilities::here(Board::Esp32c5);
        behind.state.capabilities = Some(Capabilities { version: Version::new(0, 1, 2), ..caps });
        assert_eq!(version_label(&behind, Version::new(0, 1, 3)), "v0.1.2 ↑v0.1.3");
        assert_eq!(version_label(&behind, Version::new(0, 1, 2)), "v0.1.2", "current: no arrow");
        assert_eq!(version_label(&unannounced(0x84), Version::new(0, 1, 2)), "—");
    }

    #[test]
    fn version_label_shows_version_alone_when_node_is_on_trial() {
        let mut trial = node(0x84, 0, true);
        let version = Version::new(0, 1, 2);
        let caps = Capabilities::here(Board::Esp32c5);
        trial.state.capabilities =
            Some(Capabilities { version, trial: true, five_ghz: true, ..caps });
        assert_eq!(version_label(&trial, version), format!("v{version}"));
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
