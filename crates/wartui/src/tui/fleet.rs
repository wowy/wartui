//! The channels column colours each node's share by how far its assignment has got,
//! because a MAC-layer ack is not adoption. Only the node's heartbeat, carrying the
//! epoch it holds, shows it took the frame (`NodeState::adopted`).
//!
//! | State | Wi-Fi share | Bluetooth node |
//! |---|---|---|
//! | pending: sent, not acked | yellow, the desired set then `…` | yellow `bluetooth…` |
//! | acked, not adopted | blue, the confirmed set then `…` | blue `bluetooth…` |
//! | adopted | plain, the confirmed set | cyan `bluetooth` |
//! | nothing confirmed | grey `unassigned` | grey `unassigned` |

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Row, Table, TableState};
use wartui_core::engine::{NodeView, Snapshot};
use wartui_core::record::AdminOutcome;
use wartui_proto::mac;
use wartui_proto::plan::{ChannelSet, Radio, SCAN_CHANNELS};

use super::format::ago;

/// Draw the fleet table into `area` with row `selected` under the cursor. `offset` is
/// the scroll offset from the last frame, updated to keep the cursor in view.
pub(super) fn draw_fleet(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &Snapshot,
    selected: usize,
    offset: &mut usize,
) {
    // The `lost` column appears once a node has lost a batch, like the footer's
    // `lost N`. A column of zeros wastes width.
    let show_lost = snapshot.nodes.iter().any(|n| n.state.batches_lost > 0);
    let lost_at = 4;

    let mut header = vec!["node", "rssi", "beats", "obs", "last", "channels", "state"];
    if show_lost {
        header.insert(lost_at, "lost");
    }
    let header = Row::new(header).style(Style::new().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = snapshot
        .nodes
        .iter()
        .map(|node| {
            let (state, style) = node_state(node);
            let mut cells = vec![
                Cell::from(node_label(node)),
                Cell::from(node.state.link_rssi.map_or_else(|| "—".to_owned(), |r| format!("{r}"))),
                Cell::from(node.state.heartbeats.to_string()),
                Cell::from(node.state.observations.to_string()),
                Cell::from(ago(snapshot.now_ms - node.state.last_seen_ms)),
                Cell::from(channels_cell(node)),
                Cell::from(state).style(style),
            ];
            if show_lost {
                cells.insert(lost_at, Cell::from(lost_cell(node)));
            }
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(CHANNELS_WIDTH),
        Constraint::Min(12),
    ];
    if show_lost {
        widths.insert(lost_at, Constraint::Length(5));
    }
    let title = format!(" fleet — {} of {} alive ", snapshot.alive, snapshot.nodes.len());
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut state = TableState::new()
        .with_offset(*offset)
        .with_selected((!snapshot.nodes.is_empty()).then_some(selected));
    frame.render_stateful_widget(table, area, &mut state);
    *offset = state.offset();
}

/// The channels column's width, which caps how much of the list fits.
const CHANNELS_WIDTH: u16 = 18;

/// The channels cell: what the node scans, coloured by how far the assignment has
/// got. The module docs list the states.
///
/// An empty set is the Bluetooth node's, and says so in words. As a count and list it
/// would read `0: none`, which looks like a fault rather than a job.
fn channels_cell(node: &NodeView) -> Span<'static> {
    let state = &node.state;
    if state.dirty
        && let Some(desired) = state.desired
    {
        if desired.channels.is_empty() {
            return Span::styled("bluetooth…", Style::new().fg(Color::Yellow));
        }
        return Span::styled(waiting_cell(desired.channels), Style::new().fg(Color::Yellow));
    }
    state.confirmed.map_or_else(
        || Span::styled("unassigned", Style::new().fg(Color::DarkGray)),
        |confirmed| {
            let adopted = state.adopted();
            if confirmed.channels.is_empty() {
                return if adopted {
                    Span::styled("bluetooth", Style::new().fg(Color::Cyan))
                } else {
                    Span::styled("bluetooth…", Style::new().fg(Color::Blue))
                };
            }
            if adopted {
                Span::raw(channel_cell(confirmed.channels, usize::from(CHANNELS_WIDTH)))
            } else {
                Span::styled(waiting_cell(confirmed.channels), Style::new().fg(Color::Blue))
            }
        },
    )
}

/// A set the node has not yet adopted: the channel cell with a trailing `…`.
///
/// A truncated list already ends in `…`, so it gets no second. A round-robin share
/// nearly always overflows the column, so a second would be the usual case.
fn waiting_cell(set: ChannelSet) -> String {
    let mut cell = channel_cell(set, usize::from(CHANNELS_WIDTH) - 1);
    if !cell.ends_with('…') {
        cell.push('…');
    }
    cell
}

/// Batches lost between this node and the host. Shows `—`, not 0, until the first
/// batch arrives.
///
/// Tests `observations` because every batch carries a record, and a reboot clears
/// `last_seq` but keeps the losses.
fn lost_cell(node: &NodeView) -> String {
    if node.state.observations == 0 {
        "—".to_owned()
    } else {
        node.state.batches_lost.to_string()
    }
}

/// The node's chip and the last two octets of its address: `C5 57:84`.
///
/// The chip comes from the band its heartbeats announce. A node heard only through
/// sightings shows `—` until its first heartbeat.
fn node_label(node: &NodeView) -> String {
    let chip = match node.state.capabilities.map(Radio::from) {
        Some(Radio::DualBand) => "C5",
        Some(Radio::TwoPointFour) => "C6",
        None => "— ",
    };
    format!("{chip} {}", mac::short(&node.state.mac))
}

/// Why a node cannot be given an assignment, or `None` if it can.
///
/// The text is the notice shown when `b` or `r` is refused, so it says what is wrong
/// rather than naming a state.
pub(super) fn why_not_assignable(node: &NodeView) -> Option<&'static str> {
    let state = &node.state;
    if state.capabilities.is_none() {
        return Some("has not heartbeated yet, so nothing says what its radio can tune");
    }
    if state.peer_refused {
        return Some("has no slot in the bridge's peer table, so it cannot be reached");
    }
    if !node.assignable {
        return Some("is not heartbeating, so it cannot be assigned");
    }
    None
}

/// The state column: what the operator most needs to know about a node.
///
/// `stale`: sightings arrive, heartbeats do not, usually because BLE holds the
/// antenna. The node cannot be assigned. This differs from silence, which has other
/// causes.
fn node_state(node: &NodeView) -> (String, Style) {
    let state = &node.state;
    if state.last_heartbeat.is_none() {
        return ("no heartbeat".to_owned(), Style::new().fg(Color::Yellow));
    }
    // Ahead of `stale`. A peer refusal makes a heartbeating node unassignable, and
    // the fix is on the fleet, not the node.
    if state.peer_refused {
        return ("refused".to_owned(), Style::new().fg(Color::Red));
    }
    if !node.assignable {
        return ("stale".to_owned(), Style::new().fg(Color::Yellow));
    }
    // The most actionable state. The cause is nearly always the radio being off the
    // control channel during its admin window.
    if matches!(state.last_outcome, Some(AdminOutcome::Unacked | AdminOutcome::Silent)) {
        return ("no admin ack".to_owned(), Style::new().fg(Color::Red));
    }
    // The bridge refused this send for a reason a reconnect clears (`NoPeer`, a
    // rejection), so the node stays assignable. Peer-table refusal is handled above.
    if state.last_outcome == Some(AdminOutcome::Refused) {
        return ("refused".to_owned(), Style::new().fg(Color::Red));
    }
    if state.reboots > 0 {
        return (format!("rebooted x{}", state.reboots), Style::new().fg(Color::Cyan));
    }
    ("alive".to_owned(), Style::new().fg(Color::Green))
}

/// Format a set as channel numbers, the units every other tool uses.
///
/// Runs collapse to `first-last`, so a contiguous set reads `1-11`. Round-robin shares
/// are usually scattered, so [`channel_cell`] leads with the count.
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

/// A channel set for a fixed-width cell: the count, then as much of the list as fits.
///
/// The count always fits. The list is truncated rather than wrapped, because a
/// round-robin share is a dozen scattered channels.
fn channel_cell(set: ChannelSet, width: usize) -> String {
    let head = format!("{}: ", set.len());
    let list = channel_list(set);
    if head.len() + list.len() <= width {
        return head + &list;
    }
    // Cut at a comma. Cutting `1-11,36-165` by width alone can leave `1-1`, a range
    // that does not exist. All ASCII, so bytes and columns match.
    let budget = width.saturating_sub(head.len() + 1);
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
    format!("{head}{kept}…")
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;
    use wartui_core::engine::Assignment;
    use wartui_proto::plan::{ChannelPool, IndexRun};

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    #[test]
    fn fleet_table_displays_chip_and_mac_suffix_when_rendering_nodes() {
        let snapshot = Snapshot {
            nodes: vec![node(0x84, 0, true), narrowband(0x85), unannounced(0x86)],
            tail: Vec::new(),
            ..busy()
        };
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("C5 57:84"), "a dual-band radio is a C5");
        assert!(rendered.contains("C6 57:85"), "a 2.4 GHz radio is a C6");
        assert!(rendered.contains("—  57:86"), "a node that has not heartbeated has no chip yet");
        assert!(!rendered.contains("02:00:5E:10:57"), "and no row spells out the whole address");
    }

    #[test]
    fn fleet_table_distinguishes_confirmed_from_desired_assignments_when_rendering_channel_column()
    {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("unassigned"), "a node nobody has assigned");
        assert!(rendered.contains("25: 36-165…"), "asked for, not yet acknowledged");
        assert!(rendered.contains("no admin ack"), "sent, and the node never answered");
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
    fn channel_cell_prefixes_count_and_truncates_channels_when_width_is_limited() {
        let us = ChannelPool::Us.channels();
        assert_eq!(channel_cell(us, 40), "36: 1-11,36-165");
        assert_eq!(channel_cell(us, 10), "36: 1-11…", "and never cut mid-separator");
        assert_eq!(channel_cell(us, 8), "36: …", "rather than an invented range like 1-1");
    }

    #[test]
    fn channels_cell_emits_single_ellipsis_when_pending_channel_list_is_truncated() {
        // The pending marker and the truncation marker are the same character. Two in
        // a row mean nothing more.
        let mut view = pending(0x11);
        assert_eq!(channels_cell(&view).content, "25: 36-165…");

        let mut comb = ChannelSet::empty();
        for idx in [0, 3, 6, 9, 12, 15, 18, 21] {
            comb.insert(idx);
        }
        view.state.desired.as_mut().expect("pending node has a desired assignment").channels = comb;
        let cell = channels_cell(&view).content;
        assert!(!cell.ends_with("……"), "one marker, not two: {cell}");
        assert!(cell.ends_with('…'), "still says it is pending: {cell}");
        assert!(cell.chars().count() <= usize::from(CHANNELS_WIDTH), "and still fits: {cell}");
    }

    #[test]
    fn channels_cell_renders_yellow_ellipsis_when_assignment_is_pending() {
        let view = pending(0x11);
        let cell = channels_cell(&view);
        assert!(cell.content.ends_with('…'));
        assert_eq!(cell.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn channels_cell_renders_blue_ellipsis_when_acknowledged_but_not_adopted() {
        let view = acked_not_adopted(0x11);
        assert!(!view.state.adopted());
        let cell = channels_cell(&view);
        assert!(cell.content.ends_with('…'), "the confirmed set, marked as not yet adopted");
        assert!(cell.content.starts_with('1'), "the confirmed set is still shown");
        assert_eq!(cell.style.fg, Some(Color::Blue));
    }

    #[test]
    fn channels_cell_renders_plain_when_assignment_is_adopted() {
        let view = assigned(0x11);
        assert!(view.state.adopted());
        let cell = channels_cell(&view);
        assert!(!cell.content.ends_with('…'), "nothing left to wait for");
        assert_eq!(cell.style.fg, None, "plain, not coloured");
    }

    #[test]
    fn channels_cell_renders_plain_when_desired_is_cleared_and_confirmed_is_held() {
        // A node that leaves the plan (`replan`'s departed loop, or `PeerTableFull`)
        // keeps `confirmed` while `desired` clears. `adopted` must check what it
        // still holds.
        let mut view = assigned(0x11);
        view.state.desired = None;
        assert!(view.state.adopted(), "confirmed is still held, and its epoch still matches");
        let cell = channels_cell(&view);
        assert!(!cell.content.ends_with('…'), "adopted, not stuck waiting");
        assert_eq!(cell.style.fg, None, "plain, not coloured");
    }

    #[test]
    fn channels_cell_displays_bluetooth_when_node_is_assigned_bluetooth_scan() {
        // `0: none` would read as a fault rather than a job.
        let mut view = assigned(0x11);
        let assignment =
            Assignment { channels: ChannelSet::empty(), ble: true, tx_power: 8, counter: 1 };
        view.state.confirmed = Some(assignment);
        view.state.desired = Some(assignment);
        adopt(&mut view.state);
        assert_eq!(channels_cell(&view).content, "bluetooth");

        view.state.dirty = true;
        assert_eq!(channels_cell(&view).content, "bluetooth…", "and says so while it waits");
    }

    #[test]
    fn node_state_returns_refused_label_when_node_has_no_peer_slot() {
        let view = refused(0x21);
        let (label, _) = node_state(&view);
        assert_eq!(label, "refused");
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
    fn fleet_table_shows_bluetooth_in_channel_column_when_node_holds_or_awaits_ble() {
        let mut snapshot = busy();
        // The stream pane names nodes by the same octets, on the same lines.
        snapshot.tail = Vec::new();
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

        let mut terminal = Terminal::new(TestBackend::new(160, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        let adopted_row =
            rendered.lines().find(|line| line.contains("57:84")).expect("the adopted node's row");
        assert!(adopted_row.contains("bluetooth"), "got {adopted_row}");
        assert!(!adopted_row.contains("bluetooth…"), "adopted, not pending: {adopted_row}");

        let pending_row =
            rendered.lines().find(|line| line.contains("57:87")).expect("the pending node's row");
        assert!(pending_row.contains("bluetooth…"), "got {pending_row}");
    }

    #[test]
    fn fleet_table_shows_lost_column_when_node_has_missed_batches() {
        // Rebooted since: `last_seq` is cleared, and the losses before it still show.
        let mut with_loss = node(0x84, 1, true);
        with_loss.state.last_seq = None;
        with_loss.state.batches_lost = 42;
        let mut never_sent = node(0x85, 0, true);
        never_sent.state.observations = 0;

        assert_eq!(lost_cell(&with_loss), "42");
        assert_eq!(lost_cell(&never_sent), "—", "a dash rather than a zero");

        let snapshot = Snapshot { nodes: vec![with_loss, never_sent], tail: Vec::new(), ..busy() };
        let screen = rendered(&snapshot);
        assert!(fleet_header(&screen).contains("lost"), "the column header: {screen}");
        assert!(screen.contains("42"), "the node with missed batches: {screen}");
    }

    #[test]
    fn fleet_table_hides_lost_column_when_no_node_has_missed_batches() {
        let sending = node(0x84, 1, true);
        let mut never_sent = node(0x85, 0, true);
        never_sent.state.observations = 0;
        assert_eq!(sending.state.batches_lost, 0);
        assert_eq!(never_sent.state.batches_lost, 0);

        let snapshot = Snapshot { nodes: vec![sending, never_sent], tail: Vec::new(), ..busy() };
        let screen = rendered(&snapshot);
        let header = fleet_header(&screen);
        assert!(header.contains("obs"), "the header line itself: {screen}");
        assert!(!header.contains("lost"), "{header}");
    }

    /// The fleet table's header line. Found by `rssi` and `beats`, since the stream's
    /// header has an `rssi` too and "lost" can appear elsewhere on screen.
    fn fleet_header(screen: &str) -> String {
        let header = |line: &&str| line.contains("rssi") && line.contains("beats");
        screen.lines().find(header).unwrap_or_default().to_owned()
    }

    #[test]
    fn fleet_table_scrolls_to_selected_node_when_nodes_exceed_visible_rows() {
        let mut snapshot = busy();
        snapshot.nodes = (0..15u8).map(|last| node(last, 0, true)).collect();
        snapshot.alive = 15;
        snapshot.assignable = 15;
        snapshot.tail = Vec::new();

        // Wide enough for the side-by-side layout. Short enough that the fleet box has
        // fewer data rows than the fifteen nodes above.
        let mut terminal = Terminal::new(TestBackend::new(200, 16)).expect("test backend");
        let (tx, _rx) = mpsc::channel(4);
        let press = |ui: &mut Ui, code| {
            ui.on_key(KeyEvent::new(code, KeyModifiers::NONE), &snapshot, &tx);
        };
        let mut ui = Ui::default();
        for _ in 0..14 {
            press(&mut ui, KeyCode::Down);
        }
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("57:0E"), "the selected node scrolled into view: {rendered}");
        assert!(!rendered.contains("57:00"), "the first node should have scrolled off: {rendered}");

        // Moving the cursor up one row inside the window must not move the window.
        // The offset kept in `Ui` is what stops a recompute from row 0.
        let offset_at_bottom = ui.fleet_offset();
        press(&mut ui, KeyCode::Up);
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        assert_eq!(
            ui.fleet_offset(),
            offset_at_bottom,
            "the window moved for a cursor move inside it"
        );
    }
}
