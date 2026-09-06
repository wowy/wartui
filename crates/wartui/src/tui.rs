//! The fleet view.
//!
//! Three keys here reach the air: `a` and `A` on the selected node, which ask
//! the engine for a channel-range assignment, and `p`, which hands the whole
//! fleet to the engine to partition on its own — where it starts, so `p` is
//! usually the key that takes the fleet *back*. Nothing goes out at the moment
//! a key is pressed — a node only listens in the 300 ms after its own heartbeat
//! — so the effect of a keystroke is a row that says `pending` until the next
//! sweep completes. That delay is the protocol, not lag, and the view says so
//! rather than pretending otherwise.
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
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_core::position::PositionSource;
use wartui_core::record::AdminOutcome;
use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;
use wartui_proto::plan::{IndexRun, MAX_NODES, SCAN_CHANNELS};

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
    /// Auto-assignment as it was last asked to be, until the engine's snapshot
    /// catches up. Without it two presses inside one frame would both read the
    /// same stale state and ask for the same thing twice.
    auto_wanted: Option<bool>,
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
            // The fleet, rather than one node. On, the engine cuts the pool
            // into as many ranges as there are heartbeating nodes and re-cuts
            // it whenever that number changes.
            KeyCode::Char('p') => self.toggle_auto(snapshot, commands),
            // Anything else leaves the notice alone. A refused assignment is
            // the one message the operator has to read to know why nothing
            // happened, and a key bound to nothing should not take it away.
            _ => {}
        }
    }

    /// Whether auto-assignment is on, as far as the operator can tell.
    fn auto(&mut self, snapshot: &Snapshot) -> bool {
        // The engine's answer wins as soon as it has one.
        if self.auto_wanted == Some(snapshot.auto) {
            self.auto_wanted = None;
        }
        self.auto_wanted.unwrap_or(snapshot.auto)
    }

    fn toggle_auto(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let on = !self.auto(snapshot);
        let said = match commands.try_send(Command::SetAuto(on)) {
            Ok(()) if on => {
                // Only once it is really on its way. Remembering a request the
                // engine never received would latch the view into a state
                // nothing can ever agree with: the header would say one thing,
                // `a` would be refused for the other, and `p` would flip
                // between two states the fleet is in neither of.
                self.auto_wanted = Some(on);
                "auto-assignment on: the pool is split across every heartbeating node".to_owned()
            }
            // Nothing is recalled, because there is no frame that says "scan
            // nothing" — a node keeps its last range until something replaces
            // it. Saying so beats letting an operator believe they stopped it.
            Ok(()) => {
                self.auto_wanted = Some(on);
                "auto-assignment off: each node keeps the range it holds".to_owned()
            }
            Err(_) => "the engine is not accepting commands".to_owned(),
        };
        self.say(said, snapshot);
    }

    fn assign(&mut self, range: IndexRun, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        // The engine would honour it and then take it back at the next
        // re-partition, which is a worse answer than refusing outright.
        if self.auto(snapshot) {
            self.say(
                "auto-assignment is on — press p to take the fleet back by hand".to_owned(),
                snapshot,
            );
            return;
        }
        // Twenty is the radio's peer table and therefore the fleet. Past it the
        // stagger arithmetic is describing a fleet the bridge cannot address,
        // so saying no here beats sending frames that will be refused.
        if snapshot.nodes.len() > MAX_NODES {
            self.say(
                format!(
                    "{} nodes: wartui supports {MAX_NODES}, so nothing can be assigned",
                    snapshot.nodes.len()
                ),
                snapshot,
            );
            return;
        }
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
    let faults = fault_lines(&faults(snapshot), frame.area().width);
    let footer_height = 1 + u16::try_from(faults.len()).unwrap_or(u16::MAX);
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

    // The reason a link is down used to be appended here, on the one line that
    // shares its width with the bridge's identity — so on a narrow terminal the
    // single most useful thing on screen was the part that got clipped. It
    // lives in the fault box now, which is vertical and wraps.
    let first = vec![Span::raw(link)];

    let mut second = vec![
        Span::raw(format!("pool {:?}  ", snapshot.pool)),
        planning(snapshot),
        Span::raw(format!(
            "  session {}{radio}  ",
            elapsed(snapshot.now_ms - snapshot.started_at_ms),
        )),
    ];
    second.extend(position(snapshot));

    let body = vec![Line::from(first), Line::from(second)];
    frame.render_widget(Paragraph::new(body).block(Block::bordered().title(" wartui ")), area);
}

/// Who is deciding what the fleet scans, said where the pool is said.
///
/// This is the one line that distinguishes a monitor that can assign from a
/// core replacement, so it is on screen for the whole capture rather than
/// inferable from watching ranges change on their own.
fn planning(snapshot: &Snapshot) -> Span<'static> {
    if !snapshot.auto {
        return Span::styled("manual", Style::new().fg(Color::DarkGray));
    }
    let text = match snapshot.plan {
        // Fewer nodes than the pool has runs, so one node covers them in turn
        // and coverage is intermittent rather than incorrect.
        Some(plan) if plan.rotates() => format!(
            "auto — {} of {}, rotating {}/{}",
            plan.node_count(),
            snapshot.nodes.len(),
            snapshot.phase + 1,
            plan.phase_count()
        ),
        Some(plan) => format!("auto — {} of {}", plan.node_count(), snapshot.nodes.len()),
        None if snapshot.alive > MAX_NODES => "auto — too many nodes".to_owned(),
        // Heartbeating is not on its own enough to be in a plan: an encrypted
        // node cannot be told anything, and one the bridge has no peer slot for
        // cannot be reached. The state column says which, per node.
        None if snapshot.alive > 0 => "auto — no node it can drive".to_owned(),
        None => "auto — nothing heartbeating yet".to_owned(),
    };
    Span::styled(text, Style::new().fg(Color::Green))
}

/// Where the host believes it is, said plainly and continuously.
///
/// The warning `wartui run` prints before starting is on screen for about one
/// frame before the alternate screen swallows it, which is no use to someone
/// who then watches a night of observations accumulate and only finds out at
/// export time that none of them can be uploaded. So it lives here instead,
/// where it is visible for the whole capture.
fn position(snapshot: &Snapshot) -> Vec<Span<'static>> {
    let fix = match (snapshot.position.lat, snapshot.position.lon) {
        (Some(lat), Some(lon)) => {
            Span::raw(format!("pos {lat:.5},{lon:.5} ({})", snapshot.position.source.as_str()))
        }
        _ => Span::styled(
            "pos none — these observations cannot be uploaded",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let mut spans = vec![fix];
    if let Some(gps) = &snapshot.gps {
        spans.push(receiver(gps, snapshot.position.source));
    }
    spans
}

/// What the receiver is doing, said only when it is not the thing answering.
///
/// A configured GPS that is not producing the rows' positions is the failure
/// this whole tier can have, and it is silent otherwise: the rows keep coming,
/// they simply carry the position from before the drive started. So the note
/// stays on screen until the receiver is what the chain is using.
fn receiver(gps: &GpsView, source: PositionSource) -> Span<'static> {
    // Checked before the source, because a fix stays usable for `max_age`
    // after the puck is unplugged: for those few seconds the rows really are
    // coming from the GPS and the port really is dead, and the footer is
    // already saying so.
    if let GpsStatus::Failed(reason) = &gps.status {
        return Span::styled(format!("  gps: {reason}"), Style::new().fg(Color::Yellow));
    }
    if source == PositionSource::Gps {
        let sats = match gps.status {
            GpsStatus::Fixed { satellites: Some(n) } => format!(", {n} sats"),
            _ => String::new(),
        };
        return Span::styled(format!("  gps ok{sats}"), Style::new().fg(Color::Green));
    }
    let text = match &gps.status {
        GpsStatus::Connecting => "  gps connecting",
        GpsStatus::Searching => "  gps searching",
        // Talking, has had a fix, and the chain has stopped believing it.
        GpsStatus::Fixed { .. } => "  gps fix is stale",
        // Handled above, before the source is looked at.
        GpsStatus::Failed(_) => "  gps unreadable",
    };
    let text = text.to_owned();
    Span::styled(text, Style::new().fg(Color::Yellow))
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
    // The bridge would not put it on the air at all. Nearly always a peer table
    // with no room in it, which means the fleet is over twenty nodes.
    if state.last_outcome == Some(AdminOutcome::Refused) {
        return ("refused".to_owned(), Style::new().fg(Color::Red));
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

/// How many lines of faults the footer may take before it starts summarising.
///
/// The fleet table has to keep some of the terminal.
const MAX_FAULT_LINES: usize = 3;

/// Fit the faults into lines no wider than the terminal.
///
/// Joining them all with two spaces and letting the terminal clip the overflow
/// is how the most important fault ends up invisible — and the most important
/// fault is usually the longest, because it carries an error string from the
/// operating system.
fn fault_lines(faults: &[String], width: u16) -> Vec<String> {
    // A width this small is not a terminal anyone is reading, but the
    // arithmetic below still has to terminate.
    let width = usize::from(width).max(8);
    let (mut lines, ends) = pack(faults, width);
    if lines.len() <= MAX_FAULT_LINES {
        return lines;
    }
    // Something has to give, and it is a whole line rather than the tail of one.
    // Gluing "+2 more" onto half of "Permission denied (os error 13)" leaves a
    // message that looks complete and says something else.
    let kept = MAX_FAULT_LINES - 1;
    // A fault the cut lands in the middle of is not being shown either, so it
    // counts. Otherwise the notice claims nothing is missing while the line
    // above it stops mid-word.
    let hidden = ends.iter().filter(|end| **end >= kept).count();
    lines.truncate(kept);
    lines.push(marker(hidden, width));
    lines
}

/// Lay the faults out, in however many lines that takes.
///
/// Returns the lines, and for each fault the last line it reaches — which is
/// what tells the caller what a cut would cost.
fn pack(faults: &[String], width: usize) -> (Vec<String>, Vec<usize>) {
    let mut lines: Vec<String> = Vec::new();
    let mut ends = Vec::with_capacity(faults.len());
    for fault in faults {
        let room = |line: &String| line.chars().count() + 2 + fault.chars().count() <= width;
        if lines.last().is_some_and(room) {
            if let Some(line) = lines.last_mut() {
                line.push_str("  ");
                line.push_str(fault);
            }
        } else {
            lines.extend(chunks(fault, width));
        }
        ends.push(lines.len().saturating_sub(1));
    }
    (lines, ends)
}

/// How much is not on screen, in the widest wording that fits.
fn marker(hidden: usize, width: usize) -> String {
    let count = format!("+{hidden}");
    for wording in [format!("  +{hidden} more"), format!("+{hidden} more"), count.clone()] {
        if wording.chars().count() <= width {
            return wording;
        }
    }
    // Narrower than "+1" is not a terminal either, but the notice that something
    // is missing must not itself become the thing that gets clipped.
    count.chars().take(width).collect()
}

/// One fault, split across as many lines as its own length needs.
fn chunks(fault: &str, width: usize) -> Vec<String> {
    let room = width.saturating_sub(2).max(1);
    let mut out = Vec::new();
    let mut rest: Vec<char> = fault.chars().collect();
    while !rest.is_empty() {
        let take = rest.len().min(room);
        let line: String = rest.drain(..take).collect();
        out.push(format!("  {line}"));
    }
    out
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
            " q quit  ↑↓ select  a/A assign  p auto ",
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

    // Already fitted to the width by `fault_lines`, so nothing here can be
    // clipped and no fault can push another one off the end.
    for fault in faults {
        lines.push(Line::from(Span::styled(fault.clone(), Style::new().fg(Color::Yellow))));
    }

    frame.render_widget(Paragraph::new(lines).dim(), area);
}

/// Everything that has gone wrong so far, in the order an operator would want
/// to hear it. Empty on a clean run, which is what keeps a clean run looking
/// clean.
fn faults(snapshot: &Snapshot) -> Vec<String> {
    let c = snapshot.counters;
    let mut faults = Vec::new();
    // First, because when the link is down nothing else in this list is being
    // updated and the reason is the only thing worth reading.
    if let Some(error) = &snapshot.link_error
        && !snapshot.link_up
    {
        faults.push(format!("link down: {error}"));
    }
    if snapshot.store.dropped > 0 {
        faults.push(format!("store dropped {}", snapshot.store.dropped));
    }
    if c.admin_failed > 0 {
        faults.push(format!("{} assignments unacknowledged", c.admin_failed));
    }
    if c.peer_table_full > 0 {
        faults.push(format!("peer table full: more than {MAX_NODES} nodes"));
    }
    // The planner gives up on a fleet it cannot address rather than cutting the
    // pool among nodes that will never hear the result. What is already out
    // there stays out there; it simply stops being re-cut.
    if snapshot.auto && snapshot.plan.is_none() && snapshot.alive > MAX_NODES {
        faults.push(format!(
            "{} nodes alive: over the {MAX_NODES} wartui supports, so the fleet is no longer \
             being partitioned",
            snapshot.alive
        ));
    }
    if let Some(gps) = &snapshot.gps {
        if let GpsStatus::Failed(reason) = &gps.status {
            faults.push(format!("gps unreadable: {reason}"));
        }
        // A receiver whose every line fails the checksum is talking at a rate
        // nobody is listening at. Silence and gibberish look the same in the
        // header, and only one of them is fixed with --gps-baud.
        if gps.counters.fixes == 0 && gps.counters.rejected > 20 {
            faults.push(format!(
                "gps: {} unreadable lines and no fix — wrong --gps-baud?",
                gps.counters.rejected
            ));
        }
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
    use wartui_core::gps::GpsCounters;
    use wartui_core::position::Fix;
    use wartui_proto::link::Chip;
    use wartui_proto::plan::{ChannelPool, plan};

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
            auto: false,
            plan: None,
            phase: 0,
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
                peer_table_full: 0,
                replans: 0,
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
            gps: None,
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

    const BUSY: &str = "could not open /dev/cu.usbmodem101: Device or resource busy";

    #[test]
    fn one_long_fault_is_broken_up_rather_than_cut_off() {
        // The bug this replaced: the reason a link was down was appended to the
        // header, unwrapped, so on a narrow terminal the operator saw
        // "waiting for a bridge to announce itself" and nothing else.
        let fault = format!("link down: {BUSY}");
        let lines = fault_lines(std::slice::from_ref(&fault), 40);
        assert!(lines.len() > 1, "it does not fit on one line: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let rejoined: String = lines.iter().map(|l| l.trim_start()).collect();
        assert_eq!(rejoined, fault, "every character of it is on screen somewhere");
    }

    #[test]
    fn faults_that_fit_still_share_one_line() {
        let faults = ["store dropped 7".to_owned(), "unparsed 3".to_owned()];
        assert_eq!(fault_lines(&faults, 200), vec!["  store dropped 7  unparsed 3".to_owned()]);
    }

    #[test]
    fn more_faults_than_the_footer_can_hold_are_counted_rather_than_dropped() {
        let faults: Vec<String> = (0..12).map(|n| format!("fault number {n} of twelve")).collect();
        let lines = fault_lines(&faults, 40);
        assert_eq!(lines.len(), MAX_FAULT_LINES, "the fleet table keeps the rest of the screen");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let last = lines.last().expect("a line");
        assert!(last.contains("more"), "how many are not shown: {last}");
    }

    #[test]
    fn a_fault_too_long_for_the_box_says_how_much_is_missing() {
        // Three lines of a twenty-column terminal cannot hold this, and the
        // half that names what to do about it — "Device or resource busy" — is
        // the half that goes. Cutting it silently is the bug the fault box
        // exists to fix, just at a narrower width.
        let fault = format!("link down: {BUSY}");
        let lines = fault_lines(std::slice::from_ref(&fault), 20);
        assert_eq!(lines.len(), MAX_FAULT_LINES, "{lines:?}");
        let last = lines.last().expect("a line");
        assert_eq!(last, "  +1 more", "the operator is told the reason is cut short");
        let shown: String = lines[..MAX_FAULT_LINES - 1].iter().map(|l| l.trim_start()).collect();
        assert!(fault.starts_with(&shown), "what is shown is really the start of it: {shown}");
    }

    #[test]
    fn the_notice_does_not_eat_the_message_above_it() {
        // It used to take its room out of the last line, so the reason ended
        // "n denied (os error   +1 more": a sentence that reads as complete and
        // says something the operating system never said.
        let faults = [format!("link down: {BUSY}"), "store dropped 4".to_owned()];
        let lines = fault_lines(&faults, 30);
        let last = lines.last().expect("a line");
        assert_eq!(last, "  +2 more", "both faults are cut, and it says so: {lines:?}");
        assert!(
            lines[..MAX_FAULT_LINES - 1].iter().all(|l| !l.contains('+')),
            "no fault shares a line with the notice: {lines:?}"
        );
        let shown: String = lines[..MAX_FAULT_LINES - 1].iter().map(|l| l.trim_start()).collect();
        assert!(faults[0].starts_with(&shown), "{shown}");
    }

    #[test]
    fn the_notice_itself_is_never_the_thing_that_gets_clipped() {
        // Not a terminal anyone uses, but the guarantee the footer relies on is
        // that nothing it draws is wider than the frame.
        let faults: Vec<String> = (0..2000).map(|n| format!("fault {n}")).collect();
        for width in [1_u16, 8, 12] {
            let lines = fault_lines(&faults, width);
            let room = usize::from(width).max(8);
            assert!(lines.iter().all(|l| l.chars().count() <= room), "at {width}: {lines:?}");
            assert!(lines.last().expect("a line").contains('+'), "at {width}: {lines:?}");
        }
    }

    #[test]
    fn the_reason_a_link_is_down_reaches_a_narrow_terminal() {
        let mut snapshot = empty();
        snapshot.link_error = Some(BUSY.to_owned());
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("link down"), "{screen}");
        // The tail of the reason is the part that used to be lost, and it is
        // the part that names what to do about it.
        assert!(screen.contains("busy"), "{screen}");
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

    /// A capture with a receiver attached, in whatever state it is in.
    fn with_gps(status: GpsStatus, counters: GpsCounters, source: PositionSource) -> Snapshot {
        let mut snapshot = busy();
        snapshot.position.source = source;
        if source == PositionSource::Gps {
            snapshot.position.lat = Some(48.1173);
            snapshot.position.lon = Some(11.5167);
        }
        snapshot.gps = Some(GpsView { status, counters, last_fix_ms: Some(EPOCH_MS) });
        snapshot
    }

    fn rendered(snapshot: &Snapshot) -> String {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, &Ui::default())).expect("drawing");
        terminal.backend().to_string()
    }

    #[test]
    fn a_receiver_that_is_not_the_one_answering_says_why() {
        // The failure this tier has is silent: rows keep being written, they
        // just carry the position from wherever the drive started. So a GPS
        // that is not producing the rows' positions has to say so.
        let searching =
            with_gps(GpsStatus::Searching, GpsCounters::default(), PositionSource::Static);
        assert!(rendered(&searching).contains("gps searching"));

        let stale = with_gps(
            GpsStatus::Fixed { satellites: Some(8) },
            GpsCounters::default(),
            PositionSource::Static,
        );
        // Talking, has had a fix, and the chain has stopped believing it —
        // which looks identical to a healthy receiver anywhere else on screen.
        assert!(rendered(&stale).contains("gps fix is stale"));
    }

    #[test]
    fn a_receiver_the_rows_are_using_is_named_with_its_satellite_count() {
        let fixed = with_gps(
            GpsStatus::Fixed { satellites: Some(8) },
            GpsCounters { sentences: 400, fixes: 200, rejected: 1 },
            PositionSource::Gps,
        );
        let screen = rendered(&fixed);
        assert!(screen.contains("pos 48.11730,11.51670 (gps)"), "{screen}");
        assert!(screen.contains("gps ok, 8 sats"));
    }

    #[test]
    fn a_capture_with_no_receiver_says_nothing_at_all_about_one() {
        // Most captures are static, and a permanent "gps: none" would be noise
        // on the one line that has to stay readable.
        assert!(!rendered(&busy()).contains("gps"));
    }

    #[test]
    fn a_receiver_that_has_died_does_not_read_as_healthy_while_its_fix_lasts() {
        // The seconds after the puck falls out: the rows are still the GPS's,
        // and the port is gone. Saying "gps ok" here would contradict the
        // fault the footer is showing at the same moment.
        let unplugged = with_gps(
            GpsStatus::Failed("/dev/cu.gps: Device not configured".to_owned()),
            GpsCounters { sentences: 400, fixes: 200, rejected: 0 },
            PositionSource::Gps,
        );
        let screen = rendered(&unplugged);
        assert!(screen.contains("Device not configured"), "{screen}");
        assert!(!screen.contains("gps ok"), "{screen}");
    }

    #[test]
    fn a_receiver_that_cannot_be_read_is_a_fault_rather_than_a_silence() {
        let failed = with_gps(
            GpsStatus::Failed("/dev/cu.gps: No such file or directory".to_owned()),
            GpsCounters::default(),
            PositionSource::Static,
        );
        assert!(rendered(&failed).contains("gps unreadable"));

        // Gibberish at the wrong baud rate looks exactly like silence in the
        // header, and only one of the two has a fix the operator can apply.
        let mistuned = with_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
            PositionSource::Static,
        );
        assert!(rendered(&mistuned).contains("wrong --gps-baud"));
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
    fn who_is_deciding_what_the_fleet_scans_is_on_screen_for_the_whole_capture() {
        let mut manual = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        manual.draw(|frame| draw(frame, &busy(), &Ui::default())).expect("drawing");
        assert!(manual.backend().to_string().contains("manual"));

        let mut auto = busy();
        auto.auto = true;
        auto.plan = plan(ChannelPool::Us, 4);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &auto, &Ui::default())).expect("drawing");
        assert!(terminal.backend().to_string().contains("auto — 4 of 5"));

        // One node cannot hold both runs of the US pool at once, so it covers
        // them in turn and the view says which one it is on.
        let mut rotating = busy();
        rotating.auto = true;
        rotating.plan = plan(ChannelPool::Us, 1);
        rotating.phase = 1;
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &rotating, &Ui::default())).expect("drawing");
        assert!(terminal.backend().to_string().contains("rotating 2/2"));
    }

    #[test]
    fn p_hands_the_fleet_to_the_planner_and_takes_it_back() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::SetAuto(true));

        // The snapshot the engine publishes lags a keystroke by a tick, so a
        // second press has to know what the first one asked for; otherwise both
        // read `auto: false` and both ask for the same thing.
        ui.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::SetAuto(false));
    }

    #[test]
    fn a_toggle_the_engine_never_received_does_not_latch_the_view() {
        let snapshot = busy();
        // A command channel with no room in it, which is what a wedged engine
        // looks like from here.
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Command::SetAuto(true)).expect("filling the queue");
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &snapshot, &tx);

        assert!(ui.notice(snapshot.now_ms).expect("a reason").contains("not accepting"));
        // Remembering it would leave the header saying `manual` while `a` was
        // refused for being on auto, with no way back.
        assert!(!ui.auto(&snapshot), "the fleet is where the engine says it is");
    }

    #[test]
    fn assigning_by_hand_is_refused_while_the_planner_owns_the_fleet() {
        let mut snapshot = busy();
        snapshot.auto = true;
        snapshot.plan = plan(ChannelPool::Us, 4);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &snapshot, &tx);

        // The engine would honour it and then take it back at the next
        // re-partition, which reads as the range having been ignored.
        assert!(rx.try_recv().is_err(), "nothing was queued");
        let notice = ui.notice(snapshot.now_ms).expect("a reason");
        assert!(notice.contains("press p"), "got {notice}");
    }

    #[test]
    fn a_fleet_over_the_limit_says_it_has_stopped_being_partitioned() {
        let mut snapshot = busy();
        snapshot.auto = true;
        snapshot.plan = None;
        snapshot.alive = MAX_NODES + 1;
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
        assert!(terminal.backend().to_string().contains("no longer"));
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
