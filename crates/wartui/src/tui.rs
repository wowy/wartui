//! The fleet view.
//!
//! Four keys reach the air — `a`, `A`, `b` and `p`, which the operator's manual
//! lists (`crates/wartui/README.md`). None of them transmits when pressed: a
//! node only listens in the 100 ms after its own heartbeat, so a keystroke's
//! effect is a row reading `pending` until the next sweep completes. That delay
//! is the protocol rather than lag, and the view says so.
//!
//! Rebuilt from a [`Snapshot`] the engine publishes four times a second, never
//! from a stream of observations: a busy fleet produces tens of rows a second in
//! bursts, and a UI redrawing per row would back-pressure the link.

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
use wartui_bridge::BridgeInfo;
use wartui_core::engine::{Command, NodeView, Snapshot, TailEntry};
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_core::position::PositionSource;
use wartui_core::record::AdminOutcome;
use wartui_proto::air::RecordKind;
use wartui_proto::link::{LoopPhase, Mac, ResetCause};
use wartui_proto::plan::{ChannelSet, MAX_NODES, Radio, SCAN_CHANNELS};

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
    // Built before the loop, not inside the arm below: see `crate::Terminate`.
    let mut terminate = crate::Terminate::new();
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
            // Deliberately the same exit as `q`: the terminal is restored, the
            // engine is told to stop, the last batch is committed and the port
            // is released. See `crate::Terminate`.
            () = terminate.recv() => break Ok(()),
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
            // One channel, and the whole pool. A node heartbeats once per
            // completed sweep, so narrowing it collapses the beat period and
            // widening it restores it — nothing else in the protocol reports
            // what a node is scanning.
            KeyCode::Char('a') => self.assign(narrow(snapshot), snapshot, commands),
            KeyCode::Char('A') => self.assign(widest(snapshot), snapshot, commands),
            // Honoured with auto-assignment on, unlike the assignment keys: the
            // planner partitions channels and has no opinion about BLE.
            KeyCode::Char('b') => self.toggle_ble(snapshot, commands),
            // The fleet, rather than one node.
            KeyCode::Char('p') => self.toggle_auto(snapshot, commands),
            // Anything else leaves the notice alone: a key bound to nothing must
            // not clear the one message saying why nothing happened.
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
                // Only once it is really on its way: remembering a request the
                // engine never received latches the view into a state nothing
                // can agree with.
                self.auto_wanted = Some(on);
                "auto-assignment on: the pool is split across every heartbeating node".to_owned()
            }
            // Nothing is recalled — there is no frame that says "scan nothing" —
            // so say so rather than let an operator believe they stopped it.
            Ok(()) => {
                self.auto_wanted = Some(on);
                "auto-assignment off: each node keeps the range it holds".to_owned()
            }
            Err(_) => "the engine is not accepting commands".to_owned(),
        };
        self.say(said, snapshot);
    }

    /// Move the Bluetooth scan onto the selected node, or off it.
    ///
    /// One node at a time, because that is what the engine holds. Pressing `b`
    /// on the node that already has it takes it off the fleet entirely, which
    /// is the only way back to no-BLE-anywhere.
    fn toggle_ble(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        let holds = snapshot.ble_node == Some(target);
        // Same rule as an assignment: the flag travels in the admin frame, and
        // only a heartbeat opens the window it needs.
        if !holds && let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", mac(&target)), snapshot);
            return;
        }
        // And one refusal only about Bluetooth: a node built without the `ble`
        // feature adopts the flag, acknowledges, and scans nothing, so the table
        // would name a holder and the export carry no BLE rows.
        if !holds && node.state.capabilities.is_some_and(|capabilities| !capabilities.ble) {
            self.say(
                format!(
                    "{} was built without the ble feature, so it has no bluetooth scan to run",
                    mac(&target)
                ),
                snapshot,
            );
            return;
        }
        let said = match commands
            .try_send(Command::AssignBle { mac: if holds { None } else { Some(target) } })
        {
            Ok(()) if holds => format!("{}: bluetooth off on its next heartbeat", mac(&target)),
            // The flag rides in the assignment frame, so a node without one has
            // nothing for it to ride on — under `--manual` with nothing
            // assigned, "on its next heartbeat" would never come true.
            Ok(()) if node.state.desired.is_none() && node.state.confirmed.is_none() => {
                format!("{}: bluetooth, once it has been given channels", mac(&target))
            }
            Ok(()) => format!("{}: bluetooth on its next heartbeat", mac(&target)),
            Err(_) => "the engine is not accepting commands".to_owned(),
        };
        self.say(said, snapshot);
    }

    fn assign(
        &mut self,
        channels: ChannelSet,
        snapshot: &Snapshot,
        commands: &mpsc::Sender<Command>,
    ) {
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
        // A node that will not adopt this never opens a window for it to arrive
        // through, so refusing up front with the reason beats a queue that never
        // drains.
        if let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", mac(&node.state.mac)), snapshot);
            return;
        }
        // The engine masks this too, but the notice has to name what will really
        // go out rather than what was asked for. Both pools start in 2.4 GHz and
        // both keys derive their set from the pool, so what is left is never empty.
        let channels = match node.state.capabilities {
            Some(capabilities) => Radio::from(capabilities).tunable(channels),
            None => channels,
        };
        let command = Command::Assign { mac: node.state.mac, channels };
        let said = match commands.try_send(command) {
            Ok(()) => format!(
                "{} → channel {} on its next heartbeat",
                mac(&node.state.mac),
                channel_list(channels)
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

/// The first channel of the configured pool, as a set of exactly one.
fn narrow(snapshot: &Snapshot) -> ChannelSet {
    let mut set = ChannelSet::empty();
    set.insert(snapshot.pool.runs().first().map_or(0, |run| run.start));
    set
}

/// The whole pool, which one assignment can now say — before the channel mask it
/// was the pool's longest run, never both bands.
fn widest(snapshot: &Snapshot) -> ChannelSet {
    snapshot.pool.channels()
}

/// A set of indices, said in channel numbers, which is what is written on the
/// node's own web UI and on every other tool the operator owns.
///
/// Consecutive indices collapse into `first-last`, so a contiguous assignment
/// reads as `1-11`. The planner deals round-robin, so scattered is the ordinary
/// case for a fleet of more than one — hence [`channel_cell`] leading with the
/// count.
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

/// The channel set as it goes in a fixed-width table cell.
///
/// The count comes first: it always fits, and it is the cross-check against the
/// beat column two along. The list after it is truncated rather than wrapped,
/// because a round-robin share is a dozen scattered channels.
fn channel_cell(set: ChannelSet, width: usize) -> String {
    let head = format!("{}: ", set.len());
    let list = channel_list(set);
    if head.len() + list.len() <= width {
        return head + &list;
    }
    // Cut at a comma rather than a character: truncating `1-11,36-165` by width
    // alone can end `1-1`, a range that does not exist. All ASCII, so byte
    // lengths and column widths are the same thing.
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
    // Faults get their own lines, and only when there are any: sharing the footer
    // with the counters pushed the last of them off the terminal.
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
    // fleet table needs 79 columns before its state column starts clipping.
    let [fleet, stream] = if body.width >= 119 {
        Layout::horizontal([Constraint::Length(79), Constraint::Min(40)]).areas(body)
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
        Span::raw(format!("pool {}  ", snapshot.pool)),
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
        Some(plan) => format!("auto — {} of {}", plan.node_count(), snapshot.nodes.len()),
        None if snapshot.assignable > MAX_NODES => "auto — too many nodes".to_owned(),
        // Heartbeating is not on its own enough to be in a plan: a node the
        // bridge has no peer slot for cannot be reached at all. The state
        // column says which.
        //
        // This arm is why `alive` and `assignable` are counted apart: against one
        // number a fleet that had outgrown the peer table fell through to
        // "nothing heartbeating yet" while the table showed them all doing it.
        None if snapshot.alive > 0 => "auto — no node it can drive".to_owned(),
        None => "auto — nothing heartbeating yet".to_owned(),
    };
    Span::styled(text, Style::new().fg(Color::Green))
}

/// Where the host believes it is, said plainly and continuously.
///
/// The warning `wartui run` prints at startup survives about one frame before the
/// alternate screen swallows it, which is no use to someone who finds out at
/// export time that a night of observations cannot be uploaded.
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
/// A configured GPS that is not producing the rows' positions is this tier's one
/// failure, and it is otherwise silent — the rows keep coming, carrying the
/// position from before the drive started. The note stays up until it is fixed.
fn receiver(gps: &GpsView, source: PositionSource) -> Span<'static> {
    // Checked before the source: a fix stays usable for `max_age` after the puck
    // is unplugged, so for those seconds the rows really are coming from the GPS
    // and the port really is dead.
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
        GpsStatus::Failed(_) => "  gps unreadable", // Handled above.
    };
    let text = text.to_owned();
    Span::styled(text, Style::new().fg(Color::Yellow))
}

fn draw_fleet(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, ui: &Ui) {
    let header =
        Row::new(["node", "rssi", "beats", "obs", "last", "beat", "ble", "channels", "state"])
            .style(Style::new().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = snapshot
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let (state, style) = node_state(node);
            let row = Row::new(vec![
                Cell::from(node_label(node)),
                Cell::from(node.state.link_rssi.map_or_else(|| "—".to_owned(), |r| format!("{r}"))),
                Cell::from(node.state.heartbeats.to_string()),
                Cell::from(node.state.observations.to_string()),
                Cell::from(ago(snapshot.now_ms - node.state.last_seen_ms)),
                // The measured sweep period: proportional to how many channels
                // the node scans, and so the only evidence from here that an
                // assignment was adopted rather than merely acknowledged.
                Cell::from(node.state.beat_period_ms().map_or_else(|| "—".to_owned(), period)),
                Cell::from(ble_cell(node)),
                Cell::from(channels_cell(node)),
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
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(CHANNELS_WIDTH),
        Constraint::Min(12),
    ];
    let title = format!(" fleet — {} of {} alive ", snapshot.alive, snapshot.nodes.len());
    frame.render_widget(
        Table::new(rows, widths).header(header).block(Block::bordered().title(title)),
        area,
    );
}

/// How wide the channels column is, and therefore how much of the list fits.
const CHANNELS_WIDTH: u16 = 18;

/// What the node is scanning, and whether that is known or merely wanted.
///
/// The distinction is the whole of divergence 3: a confirmed set was
/// acknowledged by the node's own radio, a pending one is waiting on a
/// heartbeat to open the window.
fn channels_cell(node: &NodeView) -> Span<'static> {
    let state = &node.state;
    if state.dirty
        && let Some(desired) = state.desired
    {
        // One character of the column is spent on the ellipsis, which is the
        // pending marker as well as the truncation marker — they cannot be
        // confused, because a pending cell is the yellow one. A cut-short list
        // ends in one already and it does both jobs at once; appending a second
        // would be the ordinary rendering rather than the rare one, because a
        // round-robin share is scattered enough to overflow the column nearly
        // always.
        let mut cell = channel_cell(desired.channels, usize::from(CHANNELS_WIDTH) - 1);
        if !cell.ends_with('…') {
            cell.push('…');
        }
        return Span::styled(cell, Style::new().fg(Color::Yellow));
    }
    state.confirmed.map_or_else(
        || Span::styled("unassigned", Style::new().fg(Color::DarkGray)),
        |confirmed| Span::raw(channel_cell(confirmed.channels, usize::from(CHANNELS_WIDTH))),
    )
}

/// Whether this node is the one carrying the Bluetooth scan.
///
/// Read off the assignment rather than the snapshot's `ble_node`, so it says what
/// the *node* is doing: the two differ for as long as an assignment takes to be
/// acknowledged, and that gap is where a coexistence failure lives.
fn ble_cell(node: &NodeView) -> Span<'static> {
    let state = &node.state;
    if state.dirty
        && let Some(desired) = state.desired
        && desired.ble != state.confirmed.is_some_and(|c| c.ble)
    {
        let label = if desired.ble { "on…" } else { "off…" };
        return Span::styled(label, Style::new().fg(Color::Yellow));
    }
    if state.confirmed.is_some_and(|c| c.ble) {
        return Span::styled("ble", Style::new().fg(Color::Cyan));
    }
    Span::raw("")
}

/// Which chip a node is and the last two octets of its address: `C5 57:84`.
///
/// The chip is read off the band its heartbeats announce, so a node heard only
/// through its sightings is `—` until its first heartbeat says.
fn node_label(node: &NodeView) -> String {
    let chip = match node.state.capabilities.map(Radio::from) {
        Some(Radio::DualBand) => "C5",
        Some(Radio::TwoPointFour) => "C6",
        None => "— ",
    };
    format!("{chip} {}", short_mac(&node.state.mac))
}

/// The last two octets of an address, which is how the boards are told apart.
fn short_mac(mac: &Mac) -> String {
    format!("{:02X}:{:02X}", mac[4], mac[5])
}

/// A sweep period, in whichever unit reads at a glance.
fn period(ms: u32) -> String {
    if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", f64::from(ms) / 1000.0) }
}

/// An estimated count, to about the precision the estimate has. The unique-address
/// figures are within about 1%, so the digits past three are noise once there are
/// enough of them to matter.
fn approx(n: u64) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let (k, m) = (n as f64 / 1e3, n as f64 / 1e6);
    // Each unit is judged on the figure as it would be printed, not the raw count, so one
    // that rounds up past its unit's range (99,950 to "100.0k") moves on to the next.
    for (value, digits, below, unit) in [(k, 1, 100.0, "k"), (k, 0, 1000.0, "k"), (m, 2, 10.0, "M")]
    {
        let text = format!("{value:.digits$}");
        if text.parse::<f64>().is_ok_and(|shown| shown < below) {
            return format!("{text}{unit}");
        }
    }
    format!("{m:.1}M")
}

/// Why a node cannot be given an assignment, or `None` if it can.
///
/// The wording is the message the operator sees on the refused keypress, so it
/// says what is wrong rather than naming a state.
fn why_not_assignable(node: &NodeView) -> Option<&'static str> {
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

/// What the operator most needs to know about a node, in one column.
///
/// "stale" is the one worth explaining: observations are arriving and heartbeats
/// are not, so the node cannot be given a range. A different fault from silence,
/// with a different cause — most often BLE holding the antenna.
fn node_state(node: &NodeView) -> (String, Style) {
    let state = &node.state;
    if state.last_heartbeat.is_none() {
        return ("no heartbeat".to_owned(), Style::new().fg(Color::Yellow));
    }
    // Ahead of `stale`: a peer refusal makes a node unassignable while its
    // heartbeats keep arriving, and the fix is on the fleet, not the node.
    if state.peer_refused {
        return ("refused".to_owned(), Style::new().fg(Color::Red));
    }
    if !node.assignable {
        return ("stale".to_owned(), Style::new().fg(Color::Yellow));
    }
    // The most actionable thing this column can say, and nearly always the same
    // cause: the radio was not on the control channel in its own admin window.
    if matches!(state.last_outcome, Some(AdminOutcome::Unacked | AdminOutcome::Silent)) {
        return ("no admin ack".to_owned(), Style::new().fg(Color::Red));
    }
    // The bridge would not put it on the air, and not for the peer table — that
    // is caught above and is terminal. `NoPeer` or a rejection, which a reconnect
    // can clear, so the node is still assignable.
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
    let title = format!(
        " unique APs ~{} — unique BLE ~{} ({} total records) ",
        approx(snapshot.unique_wifi_aps),
        approx(snapshot.unique_ble_aps),
        snapshot.counters.observations
    );
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
        // The same octets the fleet table names the node by.
        Cell::from(short_mac(&entry.node_mac)),
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
        // Beside the totals rather than in the fault box: connecting to a
        // bridge that has been buffering beside a fleet produces these as a
        // matter of course, and a line in the fault box would make the
        // ordinary case look like a broken one. It still shows before the
        // first assignment goes out, because that is the case where it is the
        // whole answer to "why has nothing been assigned yet".
        if c.admin_windows_missed > 0 {
            spans.push(Span::raw(format!("  {} held for a live window", c.admin_windows_missed)));
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
    // First: with the link down nothing else here is being updated.
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
    // The planner gives up rather than cut the pool among nodes that will never
    // hear the result; what is out there stays out there. Counted against
    // `assignable`, the list the planner is handed — a fleet of thirty of which
    // nineteen are ours partitions perfectly well.
    if snapshot.auto && snapshot.plan.is_none() && snapshot.assignable > MAX_NODES {
        faults.push(format!(
            "{} nodes alive: over the {MAX_NODES} wartui supports, so the fleet is no longer \
             being partitioned",
            snapshot.assignable
        ));
    }
    // Channels no node present has the radio for. The planner leaving them out is
    // right and completely invisible, since what remains is an ordinary partition
    // of the part the fleet can reach.
    if let Some(plan) = snapshot.plan
        && !plan.unreachable().is_empty()
    {
        faults.push(format!(
            "{} channels of the {} pool are 5 GHz and no node in this fleet has a 5 GHz radio, \
             so they are not being scanned",
            plan.unreachable().len(),
            snapshot.pool
        ));
    }
    if let Some(gps) = &snapshot.gps {
        if let GpsStatus::Failed(reason) = &gps.status {
            faults.push(format!("gps unreadable: {reason}"));
        }
        // Every line failing its checksum is a receiver talking at a rate nobody
        // is listening at, and only that is fixed with --gps-baud.
        if gps.counters.fixes == 0 && gps.counters.rejected > 20 {
            faults.push(format!(
                "gps: {} unreadable lines and no fix — wrong --gps-baud?",
                gps.counters.rejected
            ));
        }
    }
    if c.undecodable > 0 {
        faults.push(format!("undecodable {}", c.undecodable));
    }
    if c.garbled > 0 {
        faults.push(format!("garbled {}", c.garbled));
    }
    // The one fault naming a node the operator owns: a fleet half-way through a
    // reflash is invisible to the table above.
    if c.incompatible > 0 {
        faults.push(format!("{} frames from an older firmware — reflash", c.incompatible));
    }
    if c.foreign_fleet > 0 {
        faults.push(format!("{} frames from a vendor fleet", c.foreign_fleet));
    }
    if c.foreign_admin > 0 {
        faults.push(format!("{} admin frames from another core", c.foreign_admin));
    }
    // The bridge's own count runs from its boot and is dominated by what it
    // dropped before anyone was listening; only this capture's loss belongs here.
    if let Some(status) = snapshot.bridge_status
        && status.dropped_since_attach > 0
    {
        faults.push(format!("bridge dropped {}", status.dropped_since_attach));
    }
    // A restart underneath a running capture is a fault even with nothing failing
    // now: the peer table came back empty and the gap is in the data. A power-on
    // is not one — that is how a capture starts.
    if let Some(bridge) = &snapshot.bridge
        && let Some(reason) = restart_fault(bridge)
    {
        faults.push(reason);
    }
    faults
}

/// How to name a bridge restart in the fault box, or `None` for the one that
/// is not a fault.
fn restart_fault(bridge: &BridgeInfo) -> Option<String> {
    // Ahead of the cause, being the more specific statement: it reset itself.
    if matches!(bridge.last_phase, LoopPhase::TxStalled) {
        return Some("bridge rebooted itself: USB transmit had stalled".to_owned());
    }
    match bridge.reset_cause {
        ResetCause::PowerOn => None,
        ResetCause::Software => Some("bridge rebooted (firmware reset)".to_owned()),
        ResetCause::Watchdog => Some("bridge rebooted (watchdog)".to_owned()),
        ResetCause::Lockup => Some("bridge rebooted (CPU lockup)".to_owned()),
        ResetCause::Brownout => Some("bridge rebooted (brownout)".to_owned()),
        ResetCause::ClockGlitch => Some("bridge rebooted (clock glitch)".to_owned()),
        ResetCause::External => Some("bridge rebooted (reset over USB)".to_owned()),
        ResetCause::Unknown => Some("bridge rebooted (cause unknown)".to_owned()),
    }
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
    use wartui_proto::air::Capabilities;
    use wartui_proto::link::Chip;
    use wartui_proto::plan::{ChannelPool, IndexRun, Radio, plan, plan_for};

    use super::*;

    const EPOCH_MS: i64 = 1_777_642_477_000;

    fn node(last: u8, reboots: u32, assignable: bool) -> NodeView {
        let now = Now { mono: Instant::now(), unix_ms: EPOCH_MS };
        let mut state = NodeState::new([0x02, 0x00, 0x5E, 0x10, 0x57, last], now);
        state.last_seen_ms = EPOCH_MS + 60_000;
        state.last_heartbeat = Some(now.mono);
        state.counter = Some(174);
        state.reboots = reboots;
        state.heartbeats = 12;
        state.observations = 340;
        state.link_rssi = Some(-41);
        // Heartbeating unless a test says otherwise. A node that has not
        // heartbeated is not assignable at all, so building the ordinary case
        // that way would make every assignment test below a test of a refusal.
        state.capabilities = Some(Capabilities::here(true, true));
        NodeView { state, assignable }
    }

    /// A node heard only through its observations, which is an ordinary few
    /// seconds in the life of one that is about to be perfectly drivable: it
    /// reports what it found on a channel before it gets back to the control
    /// channel to heartbeat.
    fn unannounced(last: u8) -> NodeView {
        let mut view = node(last, 0, false);
        view.state.capabilities = None;
        view.state.last_heartbeat = None;
        view
    }

    /// A node whose token says 2.4 GHz only and no Bluetooth code: an ESP32-C6
    /// built without the `ble` feature.
    fn narrowband(last: u8) -> NodeView {
        let mut view = node(last, 0, true);
        view.state.capabilities = Some(Capabilities::here(false, false));
        view
    }

    /// Heartbeating stopped and nothing else is wrong. The only one of the
    /// four refusals whose cause is on the node rather than in this host.
    fn stale_node(last: u8) -> NodeView {
        node(last, 0, false)
    }

    /// Heartbeating perfectly well, with nowhere in the bridge's peer table to
    /// put it.
    fn refused(last: u8) -> NodeView {
        let mut view = node(last, 0, false);
        view.state.peer_refused = true;
        view.state.last_outcome = Some(AdminOutcome::Refused);
        view
    }

    /// A node that has been given channels and has acknowledged them.
    fn assigned(last: u8) -> NodeView {
        let mut view = node(last, 0, true);
        view.state.confirmed = Some(Assignment {
            channels: ChannelSet::from_run(IndexRun::new(0, 0)),
            ble: false,
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

    /// A node that has been given channels and has not yet had the chance to
    /// take them: the window only opens on its next heartbeat.
    fn pending(last: u8) -> NodeView {
        let mut view = node(last, 0, true);
        view.state.desired = Some(Assignment {
            channels: ChannelSet::from_run(IndexRun::new(14, 36)),
            ble: false,
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
                mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
                fw_version: "0.1.0".to_owned(),
                reset_cause: ResetCause::PowerOn,
                last_phase: LoopPhase::Unknown,
                heap_free: 65_536,
                uptime_ms: 1_000,
            }),
            link_up: true,
            link_error: None,
            pool: ChannelPool::Us,
            auto: false,
            plan: None,
            ble_node: None,
            nodes: vec![
                assigned(0x84),
                unannounced(0x85),
                node(0x86, 2, true),
                pending(0x87),
                unacked(0x88),
            ],
            alive: 4,
            assignable: 4,
            tail: (0..40)
                .map(|n| TailEntry {
                    node_mac: [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84],
                    rx_at_ms: EPOCH_MS + i64::from(n) * 1000,
                    bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, n],
                    ssid: if n % 3 == 0 { String::new() } else { format!("network {n}") },
                    security: "[WPA2_PSK]".to_owned(),
                    channel: if n % 5 == 0 { 0 } else { 6 },
                    rssi: -60,
                    kind: if n % 5 == 0 { RecordKind::Ble } else { RecordKind::Wifi },
                })
                .collect(),
            unique_wifi_aps: 32,
            unique_ble_aps: 8,
            counters: Counters {
                frames: 900,
                observations: 800,
                heartbeats: 90,
                undecodable: 1,
                incompatible: 2,
                foreign_fleet: 3,
                foreign_admin: 4,
                garbled: 5,
                admin_windows_missed: 6,
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
            assignable: 0,
            tail: Vec::new(),
            unique_wifi_aps: 0,
            unique_ble_aps: 0,
            counters: Counters::default(),
            store: StoreStats::default(),
            bridge_status: None,
            position: Fix::none(),
            ..busy()
        }
    }

    #[test]
    fn an_estimated_count_shows_only_the_digits_it_can_vouch_for() {
        assert_eq!(approx(0), "0");
        assert_eq!(approx(9_999), "9999");
        assert_eq!(approx(12_345), "12.3k");
        assert_eq!(approx(506_360), "506k");
        assert_eq!(approx(2_350_000), "2.35M");
        assert_eq!(approx(23_500_000), "23.5M");
        // Figures that round up into the next unit are shown in it.
        assert_eq!(approx(99_950), "100k");
        assert_eq!(approx(999_500), "1.00M");
        assert_eq!(approx(9_999_999), "10.0M");
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
    fn the_fleet_table_names_each_node_by_chip_and_last_two_octets() {
        let snapshot = Snapshot {
            nodes: vec![node(0x84, 0, true), narrowband(0x85), unannounced(0x86)],
            tail: Vec::new(),
            ..busy()
        };
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("C5 57:84"), "a dual-band radio is a C5");
        assert!(rendered.contains("C6 57:85"), "a 2.4 GHz radio is a C6");
        assert!(rendered.contains("—  57:86"), "a node that has not heartbeated has no chip yet");
        assert!(!rendered.contains("02:00:5E:10:57"), "and no row spells out the whole address");
    }

    #[test]
    fn a_wide_terminal_shows_the_fleet_and_the_stream_side_by_side() {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("C5 57:84"), "the fleet table");
        assert!(rendered.contains("AA:BB:CC:DD:EE"), "and the observation stream");
        assert!(rendered.contains("4 of 5 alive"));
        assert!(rendered.contains("no heartbeat"), "the one node nothing can be sent to yet");
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
        assert!(rendered.contains("older firmware"));
        assert!(rendered.contains("vendor fleet"));
    }

    const BUSY: &str = "could not open /dev/cu.usbmodem101: Device or resource busy";

    #[test]
    fn one_long_fault_is_broken_up_rather_than_cut_off() {
        // The reason a link was down used to be appended to the header
        // unwrapped, so a narrow terminal showed the first clause and nothing else.
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
        // Three lines of a twenty-column terminal cannot hold this, and the half
        // naming what to do about it is the half that would go.
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
        // It used to take its room out of the last line, leaving a sentence that
        // read as complete and said something the OS never said.
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
        // Not a terminal anyone uses, but nothing the footer draws may be wider
        // than the frame.
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
        // The tail of the reason is the part that names what to do about it.
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
    fn the_header_names_the_pool_the_way_the_readme_does() {
        // The header is the only place the pool is named on screen, and every
        // line of prose about it says "the US pool".
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Us;
        assert!(rendered(&snapshot).contains("pool US"));

        snapshot.pool = ChannelPool::All;
        assert!(rendered(&snapshot).contains("pool All"));
    }

    #[test]
    fn a_receiver_that_is_not_the_one_answering_says_why() {
        let searching =
            with_gps(GpsStatus::Searching, GpsCounters::default(), PositionSource::Static);
        assert!(rendered(&searching).contains("gps searching"));

        let stale = with_gps(
            GpsStatus::Fixed { satellites: Some(8) },
            GpsCounters::default(),
            PositionSource::Static,
        );
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
        // Most captures are static, and a permanent "gps: none" would be noise.
        assert!(!rendered(&busy()).contains("gps"));
    }

    #[test]
    fn a_receiver_that_has_died_does_not_read_as_healthy_while_its_fix_lasts() {
        // The seconds after the puck falls out: the rows are still the GPS's,
        // and the port is gone.
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

        let mistuned = with_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
            PositionSource::Static,
        );
        assert!(rendered(&mistuned).contains("wrong --gps-baud"));
    }

    /// A capture whose only fault is the bridge dropping frames.
    ///
    /// The other counters are cleared deliberately. The bridge's fault is
    /// pushed last and the footer summarises everything past
    /// `MAX_FAULT_LINES`, so a fixture carrying `busy()`'s other seven faults
    /// would be testing the packing rather than the branch.
    fn with_bridge_drops(dropped_since_attach: u32) -> Snapshot {
        let mut snapshot = busy();
        snapshot.counters = Counters::default();
        snapshot.store.dropped = 0;
        snapshot.bridge_status.as_mut().expect("busy() has a bridge").dropped_since_attach =
            dropped_since_attach;
        snapshot
    }

    #[test]
    fn frames_dropped_during_this_capture_are_a_fault_but_the_bridges_own_history_is_not() {
        let screen = rendered(&with_bridge_drops(5));
        assert!(screen.contains("bridge dropped 5"), "{screen}");

        // `busy()`'s bridge dropped 1300 frames before this host attached.
        let screen = rendered(&busy());
        assert!(!screen.contains("bridge dropped"), "{screen}");
    }

    /// A capture whose only fault is that the bridge restarted underneath it.
    fn with_restart(reset_cause: ResetCause, last_phase: LoopPhase) -> Snapshot {
        let mut snapshot = busy();
        snapshot.counters = Counters::default();
        snapshot.store.dropped = 0;
        snapshot.bridge_status.as_mut().expect("busy() has a bridge").dropped_since_attach = 0;
        let bridge = snapshot.bridge.as_mut().expect("busy() has a bridge");
        bridge.reset_cause = reset_cause;
        bridge.last_phase = last_phase;
        snapshot
    }

    #[test]
    fn a_bridge_that_restarted_is_a_fault_but_one_that_was_switched_on_is_not() {
        let screen = rendered(&with_restart(ResetCause::Watchdog, LoopPhase::Transmit));
        assert!(screen.contains("watchdog"), "{screen}");

        // Every capture starts with a bridge that was powered on. Matched on the
        // full phrase: nodes have their own `rebooted` column.
        let screen = rendered(&with_restart(ResetCause::PowerOn, LoopPhase::Unknown));
        assert!(!screen.contains("bridge rebooted"), "{screen}");
    }

    #[test]
    fn a_stalled_transmit_path_is_named_rather_than_called_a_firmware_reset() {
        let screen = rendered(&with_restart(ResetCause::Software, LoopPhase::TxStalled));
        assert!(screen.contains("USB transmit had stalled"), "{screen}");
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

        assert!(rendered.contains("unassigned"), "a node nobody has assigned");
        assert!(rendered.contains("23: 36-165…"), "asked for, not yet acknowledged");
        assert!(rendered.contains("no admin ack"), "sent, and the node never answered");
    }

    #[test]
    fn channels_are_shown_as_channel_numbers_rather_than_table_indices() {
        // Indices into SCAN_CHANNELS are an artefact of the wire format.
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(0, 0))), "1");
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(0, 10))), "1-11");
        assert_eq!(channel_list(ChannelSet::from_run(IndexRun::new(14, 36))), "36-165");
    }

    #[test]
    fn a_scattered_assignment_is_said_as_runs_rather_than_as_a_span() {
        assert_eq!(channel_list(ChannelPool::Us.channels()), "1-11,36-165");
        let mut comb = ChannelSet::empty();
        for idx in [0, 2, 4, 14, 15] {
            comb.insert(idx);
        }
        assert_eq!(channel_list(comb), "1,3,5,36-40");
        assert_eq!(channel_list(ChannelSet::empty()), "none");
    }

    #[test]
    fn a_channel_cell_leads_with_the_count_and_truncates_the_rest() {
        let us = ChannelPool::Us.channels();
        assert_eq!(channel_cell(us, 40), "34: 1-11,36-165");
        assert_eq!(channel_cell(us, 10), "34: 1-11…", "and never cut mid-separator");
        assert_eq!(channel_cell(us, 8), "34: …", "rather than an invented range like 1-1");
    }

    #[test]
    fn a_pending_cell_carries_one_ellipsis_however_long_its_list_is() {
        // The "asked for, not yet acknowledged" marker and the "more than
        // fitted" marker are the same character, and two in a row is not a
        // different meaning, just a worse-looking cell.
        let mut view = pending(0x11);
        assert_eq!(channels_cell(&view).content, "23: 36-165…");

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
    fn the_assignment_keys_pick_one_channel_and_the_whole_pool() {
        let us = busy();
        assert_eq!(narrow(&us), ChannelSet::from_run(IndexRun::new(0, 0)), "channel 1 alone");
        assert_eq!(widest(&us), ChannelPool::Us.channels());
        assert_eq!(widest(&us).len(), 34);
    }

    #[test]
    fn a_node_that_cannot_be_assigned_says_which_of_the_reasons_it_is() {
        // Three faults all end in a refused keypress and the operator's next
        // move differs for each, so one wording for all three misdirects two.
        let mut snapshot = busy();
        snapshot.nodes.push(unannounced(0x21));
        snapshot.nodes.push(stale_node(0x22));
        snapshot.nodes.push(refused(0x23));
        snapshot.nodes.sort_by_key(|n| n.state.mac);
        // `expect`, not a skip: written as a `continue` the fourth reason went
        // silently unasserted, which is how it came to be missing.
        let row = |mac_suffix: u8| {
            snapshot
                .nodes
                .iter()
                .position(|n| n.state.mac[5] == mac_suffix)
                .expect("a row for every reason")
        };

        for (row, want) in [
            (row(0x21), "heartbeated yet"),
            (row(0x22), "not heartbeating"),
            (row(0x23), "peer table"),
        ] {
            let (tx, mut rx) = mpsc::channel(4);
            let mut ui = Ui { selected: row, ..Default::default() };
            ui.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &snapshot, &tx);
            assert!(rx.try_recv().is_err(), "row {row} queued something");
            let notice = ui.notice(snapshot.now_ms).expect("a reason");
            assert!(notice.contains(want), "row {row}: wanted {want:?}, got {notice}");
        }
    }

    #[test]
    fn a_fleet_that_cannot_be_driven_is_told_so_not_that_it_is_silent() {
        // Every node heartbeating, none reachable — a fleet that has outgrown
        // the bridge's twenty peer slots, so there is nothing to partition.
        let mut snapshot = busy();
        snapshot.auto = true;
        snapshot.plan = None;
        snapshot.nodes = vec![refused(0x21), refused(0x22), refused(0x23)];
        snapshot.alive = 3;
        snapshot.assignable = 0;

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("no node it can drive"), "got {rendered}");
        assert!(!rendered.contains("nothing heartbeating"), "they are all heartbeating");
    }

    #[test]
    fn b_is_refused_on_a_node_whose_build_has_no_bluetooth_in_it() {
        let mut snapshot = busy();
        snapshot.nodes.push(narrowband(0x21));
        snapshot.nodes.sort_by_key(|n| n.state.mac);
        let row = snapshot
            .nodes
            .iter()
            .position(|n| n.state.mac[5] == 0x21)
            .expect("the narrowband node");

        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui { selected: row, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert!(rx.try_recv().is_err(), "nothing was queued");
        let notice = ui.notice(snapshot.now_ms).expect("a reason");
        assert!(notice.contains("without the ble feature"), "got {notice}");

        // And taking it back off that node is still allowed.
        let mut holding = snapshot.clone();
        holding.ble_node = Some(holding.nodes[row].state.mac);
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &holding, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::AssignBle { mac: None });
    }

    #[test]
    fn a_fleet_with_no_five_ghz_radio_says_the_pool_is_not_being_covered() {
        let mut snapshot = busy();
        snapshot.auto = true;
        snapshot.plan = plan_for(ChannelPool::Us, &[Radio::TwoPointFour; 2]);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("no node in this fleet has a 5 GHz radio"), "got {rendered}");
    }

    #[test]
    fn a_node_with_no_peer_slot_says_it_was_refused_rather_than_that_it_went_quiet() {
        let view = refused(0x21);
        let (label, _) = node_state(&view);
        assert_eq!(label, "refused");
    }

    #[test]
    fn a_node_heard_only_through_its_observations_reads_as_such() {
        // The honest case: observations have arrived and no heartbeat yet, so
        // there has been no window to send anything through.
        let view = unannounced(0x21);
        let (label, _) = node_state(&view);
        assert_eq!(label, "no heartbeat");
        assert!(view.state.observations > 0, "and it is not silent");
    }

    #[test]
    fn assigning_the_whole_pool_by_hand_promises_only_what_the_node_can_tune() {
        // `A` offers the pool and nothing re-partitions afterwards under
        // `--manual`, so this has to say the number the engine will really send.
        let mut snapshot = busy();
        snapshot.nodes = vec![narrowband(0x84)];
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &snapshot, &tx);

        let Command::Assign { channels, .. } = rx.try_recv().expect("a command") else {
            panic!("expected an assignment")
        };
        assert_eq!(channels, Radio::TwoPointFour.tunable(ChannelPool::Us.channels()));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("1-11"), "got {notice}");
        assert!(!notice.contains("36"), "the 5 GHz half is not promised: got {notice}");
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
                mac: [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84],
                channels: ChannelSet::from_run(IndexRun::new(0, 0))
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

        // A lone node holds the whole pool, and the header has nothing extra to
        // say about it.
        let mut lone = busy();
        lone.auto = true;
        lone.plan = plan(ChannelPool::Us, 1);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &lone, &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("auto — 1 of 5"));
        assert!(!rendered.contains("rotating"));
    }

    #[test]
    fn b_moves_the_bluetooth_scan_and_a_second_press_takes_it_off_the_fleet() {
        let snapshot = busy();
        let node = snapshot.nodes[0].state.mac;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::AssignBle { mac: Some(node) });

        let mut auto = busy();
        auto.auto = true;
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &auto, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::AssignBle { mac: Some(node) });

        // And pressing it on the node that already holds it is how it comes off
        // the fleet, which is the only route back to nobody scanning.
        let mut holding = busy();
        holding.ble_node = Some(node);
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &holding, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::AssignBle { mac: None });
        assert!(ui.notice(holding.now_ms).expect("a notice").contains("bluetooth off"));
    }

    #[test]
    fn the_fleet_table_says_who_holds_the_bluetooth_scan_and_who_is_about_to() {
        let mut snapshot = busy();
        snapshot.ble_node = Some(snapshot.nodes[0].state.mac);
        if let Some(confirmed) = snapshot.nodes[0].state.confirmed.as_mut() {
            confirmed.ble = true;
        }
        if let Some(desired) = snapshot.nodes[3].state.desired.as_mut() {
            desired.ble = true;
        }

        let mut terminal = Terminal::new(TestBackend::new(160, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains(" ble "), "the node whose radio confirmed it");
        assert!(rendered.contains("on…"), "and the node still waiting on its window");
    }

    #[test]
    fn p_hands_the_fleet_to_the_planner_and_takes_it_back() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::SetAuto(true));

        // The snapshot the engine publishes lags a keystroke by a tick, so a
        // second press has to know what the first one asked for.
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
        snapshot.assignable = MAX_NODES + 1;
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
