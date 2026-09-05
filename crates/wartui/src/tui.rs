//! The fleet view.
//!
//! Exactly one thing here reaches the air: `a` and `A` on the selected node,
//! which ask the engine for a channel-range assignment. Nothing goes out at the
//! moment the key is pressed — a node only listens in the 300 ms after its own
//! heartbeat — so the effect of a keystroke is a row that says `pending` until
//! the next sweep completes. That delay is the protocol, not lag, and the view
//! says so rather than pretending otherwise.
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
use wartui_core::engine::{Command, NodeView, Snapshot, TailEntry};
use wartui_core::record::AdminOutcome;
use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;
use wartui_proto::plan::{IndexRun, SCAN_CHANNELS};

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
    commands: mpsc::Sender<Command>,
    stop: oneshot::Sender<()>,
) -> Result<()> {
    let mut terminal = ratatui::try_init().context("preparing the terminal")?;
    let result = view(&mut terminal, &mut snapshot, &commands).await;
    ratatui::restore();
    // The engine is told to stop only once the terminal is back to normal, so
    // anything it logs on the way out lands on a screen the user can read.
    let _ = stop.send(());
    result
}

async fn view(
    terminal: &mut DefaultTerminal,
    snapshot: &mut watch::Receiver<Arc<Snapshot>>,
    commands: &mpsc::Sender<Command>,
) -> Result<()> {
    let (keys, running) = spawn_input();
    let mut keys = keys;
    let mut ui = Ui::default();
    let outcome = loop {
        let current = snapshot.borrow_and_update().clone();
        ui.clamp(current.nodes.len());
        if let Err(e) = terminal.draw(|frame| draw(frame, &current, &ui)) {
            break Err(e).context("drawing the fleet view");
        }

        tokio::select! {
            key = keys.recv() => match key {
                Some(key) if quits(key) => break Ok(()),
                Some(key) => ui.on_key(key, &current, commands),
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

/// What the view knows that the engine does not: which row the operator is
/// looking at, and whether they have been told off for pressing a key that
/// cannot work.
#[derive(Debug, Default)]
struct Ui {
    selected: usize,
    /// What was said, and the snapshot time it was said at.
    notice: Option<(String, i64)>,
}

/// How long a notice stays on the footer before the counters have it back.
const NOTICE_MS: i64 = 4_000;

impl Ui {
    /// Keep the cursor on a real row as nodes appear and the table grows.
    fn clamp(&mut self, node_count: usize) {
        self.selected = self.selected.min(node_count.saturating_sub(1));
    }

    fn on_key(&mut self, key: KeyEvent, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.notice = None;
                self.selected = (self.selected + 1).min(snapshot.nodes.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.notice = None;
                self.selected = self.selected.saturating_sub(1);
            }
            // One channel, and the whole pool. The pair is the Phase 4 proof:
            // a node heartbeats once per completed sweep, so narrowing it to a
            // single channel should collapse its beat period from seconds to
            // about half of one, and widening it again should put it back.
            // Nothing else in the protocol reports what a node is scanning.
            KeyCode::Char('a') => self.assign(narrow(snapshot), snapshot, commands),
            KeyCode::Char('A') => self.assign(widest(snapshot), snapshot, commands),
            // Anything else leaves the notice alone. A refused assignment is
            // the one message the operator has to read to know why nothing
            // happened, and a key bound to nothing should not take it away.
            _ => {}
        }
    }

    fn assign(&mut self, range: IndexRun, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        // A node that is not heartbeating never opens an admin window, so the
        // assignment would sit dirty forever with nothing to say why. Refusing
        // it up front, with the reason, beats a queue that never drains.
        if !node.assignable {
            self.say(
                format!("{} is not heartbeating, so it cannot be assigned", mac(&node.state.mac)),
                snapshot,
            );
            return;
        }
        let command = Command::Assign { mac: node.state.mac, range };
        let said = match commands.try_send(command) {
            Ok(()) => format!(
                "{} → channel {} on its next heartbeat",
                mac(&node.state.mac),
                channels(range)
            ),
            Err(_) => "the engine is not accepting commands".to_owned(),
        };
        self.say(said, snapshot);
    }

    fn say(&mut self, text: String, snapshot: &Snapshot) {
        self.notice = Some((text, snapshot.now_ms));
    }

    /// The notice, while it is still recent enough to be about what the
    /// operator just did.
    fn notice(&self, now_ms: i64) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| now_ms - at < NOTICE_MS)
            .map(|(text, _)| text.as_str())
    }
}

/// The first channel of the configured pool, as a range of exactly one.
fn narrow(snapshot: &Snapshot) -> IndexRun {
    let start = snapshot.pool.runs().first().map_or(0, |run| run.start);
    IndexRun::new(start, start)
}

/// The longest run in the pool — as much as a single assignment can express,
/// since `MSG_ADMIN` carries one contiguous range and the US pool has a gap.
fn widest(snapshot: &Snapshot) -> IndexRun {
    snapshot
        .pool
        .runs()
        .iter()
        .copied()
        .max_by_key(IndexRun::len)
        .unwrap_or_else(|| IndexRun::new(0, 0))
}

/// A range of indices, said in channel numbers, which is what is written on the
/// node's own web UI and on every other tool the operator owns.
fn channels(range: IndexRun) -> String {
    let first = SCAN_CHANNELS.get(usize::from(range.start)).copied().unwrap_or(0);
    let last = SCAN_CHANNELS.get(usize::from(range.end)).copied().unwrap_or(0);
    if first == last { first.to_string() } else { format!("{first}-{last}") }
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

fn draw(frame: &mut Frame<'_>, snapshot: &Snapshot, ui: &Ui) {
    // Faults get their own line, and only when there are any. Sharing the
    // footer with the counters meant that a run with several faults at once —
    // exactly the run where they matter — pushed the last of them off the end
    // of the terminal.
    let faults = faults(snapshot);
    let footer_height = if faults.is_empty() { 1 } else { 2 };
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(footer_height),
    ])
    .areas(frame.area());

    draw_header(frame, header, snapshot);

    // Side by side when there is room for both, stacked when there is not: the
    // fleet table needs 76 columns before it starts eliding MACs, which is the
    // one thing in it that cannot be guessed from context.
    let [fleet, stream] = if body.width >= 134 {
        Layout::horizontal([Constraint::Length(76), Constraint::Min(40)]).areas(body)
    } else {
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body)
    };
    draw_fleet(frame, fleet, snapshot, ui);
    draw_stream(frame, stream, snapshot);
    draw_footer(frame, footer, snapshot, ui, &faults);
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

    let mut second = vec![Span::raw(format!(
        "pool {:?}  session {}{radio}  ",
        snapshot.pool,
        elapsed(snapshot.now_ms - snapshot.started_at_ms),
    ))];
    second.push(position(snapshot));

    let body = vec![Line::from(first), Line::from(second)];
    frame.render_widget(Paragraph::new(body).block(Block::bordered().title(" wartui ")), area);
}

/// Where the host believes it is, said plainly and continuously.
///
/// The warning `wartui run` prints before starting is on screen for about one
/// frame before the alternate screen swallows it, which is no use to someone
/// who then watches a night of observations accumulate and only finds out at
/// export time that none of them can be uploaded. So it lives here instead,
/// where it is visible for the whole capture.
fn position(snapshot: &Snapshot) -> Span<'static> {
    match (snapshot.position.lat, snapshot.position.lon) {
        (Some(lat), Some(lon)) => {
            Span::raw(format!("pos {lat:.5},{lon:.5} ({})", snapshot.position.source.as_str()))
        }
        _ => Span::styled(
            "pos none — these observations cannot be uploaded",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

fn draw_fleet(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, ui: &Ui) {
    let header = Row::new(["node", "rssi", "beats", "obs", "last", "beat", "range", "state"])
        .style(Style::new().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = snapshot
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let (state, style) = node_state(node);
            let row = Row::new(vec![
                Cell::from(mac(&node.state.mac)),
                Cell::from(node.state.link_rssi.map_or_else(|| "—".to_owned(), |r| format!("{r}"))),
                Cell::from(node.state.heartbeats.to_string()),
                Cell::from(node.state.observations.to_string()),
                Cell::from(ago(snapshot.now_ms - node.state.last_seen_ms)),
                // The measured sweep period. Proportional to how many channels
                // the node is scanning, and so the only evidence available from
                // here that an assignment was actually adopted rather than
                // merely acknowledged.
                Cell::from(node.state.beat_period_ms().map_or_else(|| "—".to_owned(), period)),
                Cell::from(range_cell(node)),
                Cell::from(state).style(style),
            ]);
            if i == ui.selected {
                row.style(Style::new().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        })
        .collect();

    let widths = [
        Constraint::Length(17),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(11),
        Constraint::Min(12),
    ];
    let title = format!(" fleet — {} of {} alive ", snapshot.alive, snapshot.nodes.len());
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
}

/// What the node is scanning, and whether that is known or merely wanted.
///
/// The distinction is the whole of divergence 3. A confirmed range was
/// acknowledged by the node's own radio; a pending one has been asked for and
/// is waiting on a heartbeat to open the window. The vendor core cannot tell
/// these apart, because it clears its dirty flag from the enqueue result.
fn range_cell(node: &NodeView) -> Span<'static> {
    let state = &node.state;
    if state.dirty
        && let Some(desired) = state.desired
    {
        return Span::styled(
            format!("{}…", channels(desired.range)),
            Style::new().fg(Color::Yellow),
        );
    }
    state.confirmed.map_or_else(
        || Span::styled("unassigned", Style::new().fg(Color::DarkGray)),
        |confirmed| Span::raw(channels(confirmed.range)),
    )
}

/// A sweep period, in whichever unit reads at a glance.
fn period(ms: u32) -> String {
    if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", f64::from(ms) / 1000.0) }
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
    // An assignment that went out and was not acknowledged is the most
    // actionable thing this column can say, and the cause is nearly always the
    // same one: the node's radio was not on the control channel when its own
    // admin window was open, because NimBLE had the antenna.
    if matches!(state.last_outcome, Some(AdminOutcome::Unacked | AdminOutcome::Silent)) {
        return ("no admin ack".to_owned(), Style::new().fg(Color::Red));
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

fn draw_footer(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, ui: &Ui, faults: &[String]) {
    let c = snapshot.counters;
    let mut lines = Vec::new();

    // What just happened, when it was the operator who made it happen. This
    // takes the first line outright: a key that was refused, and why, matters
    // more in that moment than a running total that will still be there next
    // frame.
    if let Some(notice) = ui.notice(snapshot.now_ms) {
        lines.push(Line::from(Span::styled(
            format!(" {notice}"),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )));
    } else {
        let mut spans = vec![Span::styled(
            " q quit  ↑↓ select  a/A assign ",
            Style::new().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::raw(format!(
            "  frames {}  obs {}  beats {}  stored {}",
            c.frames, c.observations, c.heartbeats, snapshot.store.written
        )));
        if c.admin_sent > 0 {
            spans.push(Span::raw(format!("  admin {}/{}", c.admin_acked, c.admin_sent)));
        }
        lines.push(Line::from(spans));
    }

    if !faults.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", faults.join("  ")),
            Style::new().fg(Color::Yellow),
        )));
    }

    frame.render_widget(Paragraph::new(lines).dim(), area);
}

/// Everything that has gone wrong so far, in the order an operator would want
/// to hear it. Empty on a clean run, which is what keeps a clean run looking
/// clean.
fn faults(snapshot: &Snapshot) -> Vec<String> {
    let c = snapshot.counters;
    let mut faults = Vec::new();
    if snapshot.store.dropped > 0 {
        faults.push(format!("store dropped {}", snapshot.store.dropped));
    }
    if c.admin_failed > 0 {
        faults.push(format!("{} assignments unacknowledged", c.admin_failed));
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
    faults
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
    use wartui_core::engine::{Assignment, BridgeStatus, Counters, NodeState, Now, StoreStats};
    use wartui_core::position::{Fix, PositionSource};
    use wartui_proto::link::Chip;
    use wartui_proto::plan::ChannelPool;

    use super::*;

    const EPOCH_MS: i64 = 1_777_642_477_000;

    fn node(last: u8, encrypted: bool, reboots: u32, assignable: bool) -> NodeView {
        let now = Now { mono: Instant::now(), unix_ms: EPOCH_MS };
        let mut state = NodeState::new([0x38, 0x44, 0xBE, 0x1F, 0x57, last], now);
        state.last_seen_ms = EPOCH_MS + 60_000;
        state.last_heartbeat = Some(now.mono);
        state.counter = Some(174);
        state.reboots = reboots;
        state.heartbeats = 12;
        state.observations = 340;
        state.link_rssi = Some(-41);
        state.encrypted = encrypted;
        NodeView { state, assignable }
    }

    /// A node that has been given a range and has acknowledged it.
    fn assigned(last: u8) -> NodeView {
        let mut view = node(last, false, 0, true);
        view.state.confirmed = Some(Assignment {
            range: IndexRun::new(0, 0),
            node_index: 0,
            node_count: 1,
            counter: 9,
        });
        view.state.last_outcome = Some(AdminOutcome::Acked);
        view.state.last_latency_us = Some(4_200);
        view
    }

    /// A node that was sent an assignment and whose radio never acknowledged
    /// it — in practice, BLE holding the antenna through the admin window.
    fn unacked(last: u8) -> NodeView {
        let mut view = pending(last);
        view.state.last_outcome = Some(AdminOutcome::Unacked);
        view.state.admin_attempts = 3;
        view
    }

    /// A node that has been given a range and has not yet had the chance to
    /// take it: the window only opens on its next heartbeat.
    fn pending(last: u8) -> NodeView {
        let mut view = node(last, false, 0, true);
        view.state.desired = Some(Assignment {
            range: IndexRun::new(14, 36),
            node_index: 0,
            node_count: 1,
            counter: 10,
        });
        view.state.dirty = true;
        view
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
                assigned(0x84),
                node(0x85, true, 0, false),
                node(0x86, false, 2, true),
                pending(0x87),
                unacked(0x88),
            ],
            alive: 4,
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
                admin_sent: 3,
                admin_acked: 2,
                admin_failed: 1,
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
            position: Fix {
                lat: Some(37.7749),
                lon: Some(-122.4194),
                alt: Some(16.0),
                accuracy: None,
                source: PositionSource::Static,
                at_ms: None,
            },
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
            position: Fix::none(),
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
                terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
            }
        }
    }

    #[test]
    fn a_wide_terminal_shows_the_fleet_and_the_stream_side_by_side() {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("38:44:BE:1F:57:84"), "the fleet table");
        assert!(rendered.contains("AA:BB:CC:DD:EE"), "and the observation stream");
        assert!(rendered.contains("4 of 5 alive"));
        assert!(rendered.contains("encrypted"), "the one node wartui cannot talk to");
    }

    #[test]
    fn faults_are_only_shown_once_they_have_happened() {
        let mut clean = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        clean.draw(|frame| draw(frame, &empty(), &Ui::default())).expect("drawing");
        assert!(!clean.backend().to_string().contains("dropped"));

        let mut faulty = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        faulty.draw(|frame| draw(frame, &busy(), &Ui::default())).expect("drawing");
        let rendered = faulty.backend().to_string();
        assert!(rendered.contains("store dropped 7"));
        assert!(rendered.contains("admin frames from another core"));
    }

    #[test]
    fn a_link_that_is_down_says_why() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &empty(), &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("waiting for a bridge"));
        assert!(rendered.contains("no bridge found"));
    }

    #[test]
    fn an_unpositioned_capture_says_so_for_the_whole_run() {
        let mut positioned = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        positioned.draw(|frame| draw(frame, &busy(), &Ui::default())).expect("drawing");
        assert!(positioned.backend().to_string().contains("pos 37.77490,-122.41940 (static)"));

        let mut without = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        without.draw(|frame| draw(frame, &empty(), &Ui::default())).expect("drawing");
        assert!(without.backend().to_string().contains("pos none"));
    }

    #[test]
    fn q_escape_and_ctrl_c_all_quit() {
        assert!(quits(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    }

    #[test]
    fn the_fleet_table_distinguishes_a_confirmed_range_from_a_wanted_one() {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        // Divergence 3 made visible. A confirmed range was acknowledged by the
        // node's own radio; a pending one has been asked for and is waiting on
        // a heartbeat to open the 300 ms window. The vendor core cannot tell
        // these apart, because it clears its dirty flag from the enqueue.
        assert!(rendered.contains("unassigned"), "a node nobody has assigned");
        assert!(rendered.contains("36-165…"), "asked for, not yet acknowledged");
        assert!(rendered.contains("no admin ack"), "sent, and the node never answered");
    }

    #[test]
    fn ranges_are_shown_as_channel_numbers_rather_than_table_indices() {
        // Indices into SCAN_CHANNELS are an artefact of the wire format. The
        // number written on the node's own web UI is the channel.
        assert_eq!(channels(IndexRun::new(0, 0)), "1");
        assert_eq!(channels(IndexRun::new(0, 10)), "1-11");
        assert_eq!(channels(IndexRun::new(14, 36)), "36-165");
    }

    #[test]
    fn the_assignment_keys_pick_one_channel_and_the_widest_run_in_the_pool() {
        let us = busy();
        assert_eq!(narrow(&us), IndexRun::new(0, 0), "channel 1 alone");
        // `MSG_ADMIN` carries one contiguous range, and the US pool has a gap
        // at indices 11-13, so the widest a single assignment can be is one run.
        assert_eq!(widest(&us), IndexRun::new(14, 36));
    }

    #[test]
    fn a_node_that_is_not_heartbeating_is_refused_with_the_reason() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        // The encrypted node, which is not assignable. An assignment queued
        // against it would sit dirty forever, because only a heartbeat opens
        // the window it needs.
        let mut ui = Ui { selected: 1, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "nothing was queued");
        let notice = ui.notice(snapshot.now_ms).expect("a reason");
        assert!(notice.contains("not heartbeating"), "got {notice}");
    }

    #[test]
    fn assigning_the_selected_node_queues_one_command_and_says_so() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(
            rx.try_recv().expect("a command"),
            Command::Assign {
                mac: [0x38, 0x44, 0xBE, 0x1F, 0x57, 0x84],
                range: IndexRun::new(0, 0)
            }
        );
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("on its next heartbeat"), "got {notice}");
        // And it goes away again, rather than sitting on the footer for the
        // rest of the capture in place of the counters.
        assert!(ui.notice(snapshot.now_ms + NOTICE_MS).is_none());
    }

    #[test]
    fn the_cursor_stays_on_a_real_row_as_the_fleet_grows_and_shrinks() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        for _ in 0..10 {
            ui.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snapshot, &tx);
        }
        assert_eq!(ui.selected, snapshot.nodes.len() - 1);

        // A node ageing out of the table must not leave the cursor pointing
        // past the end of it.
        ui.clamp(1);
        assert_eq!(ui.selected, 0);
    }
}
