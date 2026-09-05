//! The read-only fleet view.
//!
//! Nothing in here can transmit, and there is no key bound to anything but
//! quitting. That is the point of the phase: a tool that can be pointed at a
//! live fleet with no possibility of disturbing it, so the store, the decoder
//! and the export can be trusted before anything starts assigning channels.
//!
//! The view is rebuilt from a [`Snapshot`] the engine publishes four times a
//! second and never from a stream of individual observations. A busy fleet can
//! produce tens of rows a second in bursts; a terminal cannot usefully redraw
//! that often, and a UI that tried would be back-pressuring the link.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_core::engine::{NodeView, Snapshot, TailEntry};
use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;

/// How long the input thread waits for a keypress before checking whether it
/// should stop. Long enough not to spin, short enough that quitting is instant.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// Run the view until the operator quits or the engine stops.
///
/// Takes ownership of `stop` so leaving by any route — a keypress, ctrl-c, or
/// the engine ending on its own — shuts the capture down the same way, with the
/// store's last batch committed.
pub async fn run(
    mut snapshot: watch::Receiver<Arc<Snapshot>>,
    stop: oneshot::Sender<()>,
) -> Result<()> {
    let mut terminal = ratatui::try_init().context("preparing the terminal")?;
    let result = view(&mut terminal, &mut snapshot).await;
    ratatui::restore();
    // The engine is told to stop only once the terminal is back to normal, so
    // anything it logs on the way out lands on a screen the user can read.
    let _ = stop.send(());
    result
}

async fn view(
    terminal: &mut DefaultTerminal,
    snapshot: &mut watch::Receiver<Arc<Snapshot>>,
) -> Result<()> {
    let (keys, running) = spawn_input();
    let mut keys = keys;
    let outcome = loop {
        let current = snapshot.borrow_and_update().clone();
        if let Err(e) = terminal.draw(|frame| draw(frame, &current)) {
            break Err(e).context("drawing the fleet view");
        }

        tokio::select! {
            key = keys.recv() => match key {
                Some(key) if quits(key) => break Ok(()),
                Some(_) => {}
                // The input thread died; carrying on would leave a view nobody
                // can quit.
                None => break Ok(()),
            },
            changed = snapshot.changed() => {
                if changed.is_err() {
                    break Ok(());   // The engine stopped.
                }
            }
        }
    };
    running.store(false, Ordering::Relaxed);
    outcome
}

/// Keys are read on their own thread rather than through an async stream: a
/// blocking `poll` on stdin is exactly what the crossterm API is built for, and
/// it keeps the terminal off the tokio runtime entirely.
fn spawn_input() -> (mpsc::Receiver<KeyEvent>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel(16);
    let running = Arc::new(AtomicBool::new(true));
    std::thread::Builder::new()
        .name("wartui-input".to_owned())
        .spawn({
            let running = Arc::clone(&running);
            move || {
                while running.load(Ordering::Relaxed) {
                    match event::poll(INPUT_POLL) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(_) => return,
                    }
                    if let Ok(TermEvent::Key(key)) = event::read()
                        && tx.blocking_send(key).is_err()
                    {
                        return;
                    }
                }
            }
        })
        .map_or_else(|_| tracing_spawn_failed(), |_| ());
    (rx, running)
}

/// Losing the input thread is survivable — ctrl-c still works — but silence
/// would leave the operator pressing `q` at a view that will not close.
fn tracing_spawn_failed() {
    eprintln!("could not start the keyboard thread; use ctrl-c to quit");
}

fn quits(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')))
}

fn draw(frame: &mut Frame<'_>, snapshot: &Snapshot) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(6), Constraint::Length(1)])
            .areas(frame.area());

    draw_header(frame, header, snapshot);

    // Side by side when there is room for both, stacked when there is not: the
    // fleet table needs 62 columns before it starts eliding MACs, which is the
    // one thing in it that cannot be guessed from context.
    let [fleet, stream] = if body.width >= 120 {
        Layout::horizontal([Constraint::Length(62), Constraint::Min(40)]).areas(body)
    } else {
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body)
    };
    draw_fleet(frame, fleet, snapshot);
    draw_stream(frame, stream, snapshot);
    draw_footer(frame, footer, snapshot);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let link = if let Some(bridge) = &snapshot.bridge {
        let state = if snapshot.link_up { "up" } else { "down" };
        format!(
            "bridge {} on {:?}, firmware {} — link {state}",
            mac(&bridge.mac),
            bridge.chip,
            bridge.fw_version
        )
    } else {
        "waiting for a bridge to announce itself".to_owned()
    };

    let radio = snapshot.bridge_status.map_or_else(
        || "  channel ?".to_owned(),
        |s| format!("  channel {}  peers {}  bridge rx {}", s.channel, s.peer_count, s.rx_count),
    );

    let mut first = vec![Span::raw(link)];
    if let Some(error) = &snapshot.link_error
        && !snapshot.link_up
    {
        first.push(Span::styled(format!("  ({error})"), Style::new().fg(Color::Red)));
    }

    let second = format!(
        "pool {:?}  session {}{radio}",
        snapshot.pool,
        elapsed(snapshot.now_ms - snapshot.started_at_ms),
    );

    let body = vec![Line::from(first), Line::from(second)];
    frame.render_widget(Paragraph::new(body).block(Block::bordered().title(" wartui ")), area);
}

fn draw_fleet(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let header = Row::new(["node", "rssi", "beats", "obs", "last", "state"])
        .style(Style::new().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = snapshot
        .nodes
        .iter()
        .map(|node| {
            let (state, style) = node_state(node);
            Row::new(vec![
                Cell::from(mac(&node.state.mac)),
                Cell::from(node.state.link_rssi.map_or_else(|| "—".to_owned(), |r| format!("{r}"))),
                Cell::from(node.state.heartbeats.to_string()),
                Cell::from(node.state.observations.to_string()),
                Cell::from(ago(snapshot.now_ms - node.state.last_seen_ms)),
                Cell::from(state).style(style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(17),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Min(12),
    ];
    let title = format!(" fleet — {} of {} alive ", snapshot.alive, snapshot.nodes.len());
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
}

/// What the operator most needs to know about a node, in one column.
///
/// Encryption comes first because it is the only state wartui cannot do
/// anything about from here: an encrypted node will never answer, and the fix
/// is in that node's own web UI.
///
/// "stale" is the one worth explaining. It means the node is still being heard
/// — observations are arriving — but its heartbeats are not, so it cannot be
/// given a channel range. That is a different fault from silence and has a
/// different cause, most often BLE coexistence on the node holding the radio
/// through its admin window.
fn node_state(node: &NodeView) -> (String, Style) {
    let state = &node.state;
    if state.encrypted {
        return ("encrypted".to_owned(), Style::new().fg(Color::Magenta));
    }
    if state.last_heartbeat.is_none() {
        return ("no heartbeat".to_owned(), Style::new().fg(Color::Yellow));
    }
    if !node.assignable {
        return ("stale".to_owned(), Style::new().fg(Color::Yellow));
    }
    if state.reboots > 0 {
        return (format!("rebooted x{}", state.reboots), Style::new().fg(Color::Cyan));
    }
    ("alive".to_owned(), Style::new().fg(Color::Green))
}

fn draw_stream(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let header = Row::new(["time", "node", "bssid", "ch", "rssi", "ssid"])
        .style(Style::new().add_modifier(Modifier::BOLD));

    // Newest last, and only as many as fit: the tail is capped at 200 but the
    // pane is usually a couple of dozen lines, and rendering rows that will be
    // scrolled off is work nobody sees.
    let visible = usize::from(area.height.saturating_sub(3));
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
    let title = format!(" observations — {} kept ", snapshot.counters.observations);
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
}

fn observation_row(entry: &TailEntry) -> Row<'static> {
    let kind = match entry.kind {
        RecordKind::Wifi => Style::new().fg(Color::Green),
        RecordKind::Ble => Style::new().fg(Color::Blue),
    };
    let ssid = if entry.ssid.is_empty() {
        Span::styled("<hidden>", Style::new().fg(Color::DarkGray))
    } else {
        Span::raw(entry.ssid.clone())
    };
    Row::new(vec![
        Cell::from(clock(entry.rx_at_ms)),
        // The last two bytes of the reporting node's MAC. Enough to tell one
        // node's rows from another's at a glance; the full address is a pane
        // away in the fleet table.
        Cell::from(format!("{:02X}:{:02X}", entry.node_mac[4], entry.node_mac[5])),
        Cell::from(mac(&entry.bssid)).style(kind),
        Cell::from(if entry.channel == 0 { "—".to_owned() } else { entry.channel.to_string() }),
        Cell::from(entry.rssi.to_string()),
        Cell::from(Line::from(ssid)),
    ])
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let c = snapshot.counters;
    let mut spans = vec![Span::styled(
        " q quit ",
        Style::new().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(format!(
        "  frames {}  obs {}  beats {}  stored {}",
        c.frames, c.observations, c.heartbeats, snapshot.store.written
    )));

    // Everything below is a fault of some kind and only appears when it has
    // happened, so a clean run reads as a clean footer.
    let mut faults = Vec::new();
    if snapshot.store.dropped > 0 {
        faults.push(format!("store dropped {}", snapshot.store.dropped));
    }
    if c.unparsed > 0 {
        faults.push(format!("unparsed {}", c.unparsed));
    }
    if c.undecodable > 0 {
        faults.push(format!("undecodable {}", c.undecodable));
    }
    if c.garbled > 0 {
        faults.push(format!("garbled {}", c.garbled));
    }
    if c.core_frames > 0 {
        faults.push(format!("{} encrypted-node frames", c.core_frames));
    }
    if c.foreign_admin > 0 {
        faults.push(format!("{} admin frames from another core", c.foreign_admin));
    }
    // The bridge's own count runs from its boot and is usually dominated by
    // what it dropped before anyone was listening. Only the part this capture
    // lost belongs in front of the operator.
    if let Some(status) = snapshot.bridge_status
        && status.dropped_since_attach > 0
    {
        faults.push(format!("bridge dropped {}", status.dropped_since_attach));
    }
    if !faults.is_empty() {
        spans
            .push(Span::styled(format!("  {}", faults.join("  ")), Style::new().fg(Color::Yellow)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).dim(), area);
}

fn mac(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn clock(unix_ms: i64) -> String {
    DateTime::from_timestamp_millis(unix_ms).map_or_else(
        || "--:--:--".to_owned(),
        |dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string(),
    )
}

fn elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

fn ago(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 100 { format!("{secs}s") } else { format!("{}m", secs / 60) }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use wartui_bridge::BridgeInfo;
    use wartui_core::engine::{BridgeStatus, Counters, NodeState, StoreStats};
    use wartui_proto::link::Chip;
    use wartui_proto::plan::ChannelPool;

    use super::*;

    const EPOCH_MS: i64 = 1_777_642_477_000;

    fn node(last: u8, encrypted: bool, reboots: u32, assignable: bool) -> NodeView {
        let state = NodeState {
            mac: [0x38, 0x44, 0xBE, 0x1F, 0x57, last],
            first_seen_ms: EPOCH_MS,
            last_seen_ms: EPOCH_MS + 60_000,
            last_seen: Instant::now(),
            last_heartbeat: Some(Instant::now()),
            counter: Some(174),
            reboots,
            heartbeats: 12,
            observations: 340,
            link_rssi: Some(-41),
            encrypted,
        };
        NodeView { state, assignable }
    }

    fn busy() -> Snapshot {
        Snapshot {
            bridge: Some(BridgeInfo {
                chip: Chip::Esp32C6,
                mac: [0x98, 0xA3, 0x16, 0x8E, 0x9D, 0x24],
                fw_version: "0.1.0".to_owned(),
            }),
            link_up: true,
            link_error: None,
            pool: ChannelPool::Us,
            nodes: vec![
                node(0x84, false, 0, true),
                node(0x85, true, 0, false),
                node(0x86, false, 2, true),
            ],
            alive: 2,
            tail: (0..40)
                .map(|n| TailEntry {
                    node_mac: [0x38, 0x44, 0xBE, 0x1F, 0x57, 0x84],
                    rx_at_ms: EPOCH_MS + i64::from(n) * 1000,
                    bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, n],
                    ssid: if n % 3 == 0 { String::new() } else { format!("network {n}") },
                    security: "[WPA2_PSK]".to_owned(),
                    channel: if n % 5 == 0 { 0 } else { 6 },
                    rssi: -60,
                    kind: if n % 5 == 0 { RecordKind::Ble } else { RecordKind::Wifi },
                })
                .collect(),
            counters: Counters {
                frames: 900,
                observations: 800,
                heartbeats: 90,
                undecodable: 1,
                unparsed: 2,
                core_frames: 3,
                foreign_admin: 4,
                garbled: 5,
            },
            store: StoreStats { written: 800, dropped: 7 },
            bridge_status: Some(BridgeStatus {
                channel: 6,
                peer_count: 0,
                rx_count: 900,
                dropped_tx: 1300,
                dropped_since_attach: 0,
                uptime_ms: 60_000,
            }),
            started_at_ms: EPOCH_MS,
            now_ms: EPOCH_MS + 60_000,
        }
    }

    fn empty() -> Snapshot {
        Snapshot {
            bridge: None,
            link_up: false,
            link_error: Some("no bridge found".to_owned()),
            nodes: Vec::new(),
            alive: 0,
            tail: Vec::new(),
            counters: Counters::default(),
            store: StoreStats::default(),
            bridge_status: None,
            ..busy()
        }
    }

    /// Rendering must not panic at any size the terminal might be.
    ///
    /// A layout that divides by a zero-width area or indexes past a short one
    /// takes the whole capture down with it, in the alternate screen, where the
    /// backtrace is unreadable. These sizes are cheap insurance against that.
    #[test]
    fn every_plausible_terminal_size_renders() {
        for snapshot in [busy(), empty()] {
            for (width, height) in [(200, 50), (120, 30), (80, 24), (40, 10), (20, 5), (6, 3)] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("test backend");
                terminal.draw(|frame| draw(frame, &snapshot)).expect("drawing");
            }
        }
    }

    #[test]
    fn a_wide_terminal_shows_the_fleet_and_the_stream_side_by_side() {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("38:44:BE:1F:57:84"), "the fleet table");
        assert!(rendered.contains("AA:BB:CC:DD:EE"), "and the observation stream");
        assert!(rendered.contains("2 of 3 alive"));
        assert!(rendered.contains("encrypted"), "the one node wartui cannot talk to");
    }

    #[test]
    fn faults_are_only_shown_once_they_have_happened() {
        let mut clean = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        clean.draw(|frame| draw(frame, &empty())).expect("drawing");
        assert!(!clean.backend().to_string().contains("dropped"));

        let mut faulty = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        faulty.draw(|frame| draw(frame, &busy())).expect("drawing");
        let rendered = faulty.backend().to_string();
        assert!(rendered.contains("store dropped 7"));
        assert!(rendered.contains("admin frames from another core"));
    }

    #[test]
    fn a_link_that_is_down_says_why() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &empty())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("waiting for a bridge"));
        assert!(rendered.contains("no bridge found"));
    }

    #[test]
    fn q_escape_and_ctrl_c_all_quit() {
        assert!(quits(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    }
}
