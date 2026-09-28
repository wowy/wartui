//! The fleet view.
//!
//! See the operator's manual (`crates/wartui/README.md`) for a usage guide.
//!
//! Rebuilt from a [`Snapshot`] the engine publishes four times a second, never
//! from a stream of observations: a busy fleet produces tens of rows a second in
//! bursts, and a UI redrawing per row would back-pressure the link.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::BridgeInfo;
use wartui_core::engine::{Command, Counters, NodeView, Snapshot, TailEntry};
use wartui_core::gps::{GpsStatus, GpsView};
// Shared with the bridge's panel rather than written twice: the two reporting
// different numbers for one estimate is a bug nobody would think to look for.
use wartui_core::panel::approx;
use wartui_core::position::PositionSource;
use wartui_core::record::AdminOutcome;
use wartui_proto::air::RecordKind;
use wartui_proto::link::{LoopPhase, Mac, ResetCause};
use wartui_proto::plan::{self, ChannelPool, ChannelSet, MAX_NODES, Radio, SCAN_CHANNELS};

use crate::config;
use crate::run::PoolArg;

/// How long the input thread waits for a keypress before checking whether it
/// should stop. Long enough not to spin, short enough that quitting is instant.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// What the settings modal needs beyond what a [`Snapshot`] carries.
///
/// Resolved once, in `main`, and carried into the view rather than re-derived
/// from a snapshot: where to save is a fact about how this process was
/// started, not about the fleet.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Where `wartui.toml` would be written, or `None` when there is nowhere
    /// to save it — no default location found and no `--config` given.
    pub config_path: Option<PathBuf>,
}

/// Run the view until the operator quits or the engine stops.
///
/// Takes ownership of `stop` so leaving by any route — a keypress, ctrl-c, or
/// the engine ending on its own — shuts the capture down the same way, with the
/// store's last batch committed.
pub async fn run(
    mut snapshot: watch::Receiver<Arc<Snapshot>>,
    commands: mpsc::Sender<Command>,
    stop: oneshot::Sender<()>,
    settings: Settings,
) -> Result<()> {
    let mut terminal = ratatui::try_init().context("preparing the terminal")?;
    let result = view(&mut terminal, &mut snapshot, &commands, settings).await;
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
    settings: Settings,
) -> Result<()> {
    let (keys, running) = spawn_input();
    let mut keys = keys;
    let mut ui = Ui { settings, ..Ui::default() };
    // Built before the loop, not inside the arm below: see `crate::Terminate`.
    let mut terminate = crate::Terminate::new();
    let outcome = loop {
        let current = snapshot.borrow_and_update().clone();
        ui.clamp(current.nodes.len());
        if let Err(e) = terminal.draw(|frame| draw(frame, &current, &mut ui)) {
            break Err(e).context("drawing the fleet view");
        }

        tokio::select! {
            key = keys.recv() => match key {
                // ctrl-c always quits, modal or not.
                Some(key) if is_ctrl_c(key) => break Ok(()),
                // An open modal gets the key ahead of `quits()`: `esc`/`q` close
                // it rather than the view while it is open.
                Some(key) if ui.modal.is_some() => ui.on_modal_key(key, &current, commands),
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

/// Which row the operator is looking at, and whether they have been notified of a key
/// press that cannot work.
#[derive(Debug, Default)]
struct Ui {
    selected: usize,
    /// The fleet table's scroll offset, carried from the previous frame so a
    /// stateful `Table` moves the window only when the cursor reaches its
    /// edge rather than recomputing from row 0 — which would pin the cursor
    /// to the bottom of the box and turn `k` into a scroll instead of a move.
    fleet_offset: usize,
    /// The notice and the snapshot time it was sent.
    notice: Option<(String, i64)>,
    /// The settings modal, open or closed.
    modal: Option<ConfigModal>,
    /// Where to save. Fixed for the life of the view.
    settings: Settings,
}

/// One row the settings modal can move between. A new setting adds a variant
/// here and a row in [`draw_settings_modal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Pool,
    Fleet,
    Bridge,
}

/// Every row of the settings modal, top to bottom, as `next`/`prev` walk them.
const FIELDS: [Field; 3] = [Field::Pool, Field::Fleet, Field::Bridge];

impl Field {
    /// The row below this one, clamped: the last row stays put rather than
    /// wrapping to the first.
    fn next(self) -> Self {
        let index = FIELDS.iter().position(|&field| field == self).unwrap_or(0);
        FIELDS[(index + 1).min(FIELDS.len() - 1)]
    }

    /// The row above this one, clamped: the first row stays put rather than
    /// wrapping to the last.
    fn prev(self) -> Self {
        let index = FIELDS.iter().position(|&field| field == self).unwrap_or(0);
        FIELDS[index.saturating_sub(1)]
    }
}

/// The settings modal's own state while it is open, seeded from the snapshot
/// that was current when it opened and edited independently of it from then on.
#[derive(Debug, Clone, Copy)]
struct ConfigModal {
    selected: Field,
    /// Whole dBm, the units the operator sees and `config::TX_POWER_DBM` bounds.
    fleet_dbm: i8,
    /// Whole dBm, the bridge's effective power — always a number, since the
    /// bridge row shows what is in force rather than whether anything holds it.
    bridge_dbm: i8,
    /// The pool row's current value, edited by `step`.
    pool: PoolArg,
    /// The pool that is actually running, fixed at the value `pool` was seeded
    /// with: the pool cannot change live, so this is what `save_outcome`
    /// compares `pool` against to say a restart is needed.
    running_pool: PoolArg,
}

/// `All → Eu → Us`, the order the pool row steps through.
const POOL_STEPS: [PoolArg; 3] = [PoolArg::All, PoolArg::Eu, PoolArg::Us];

/// Step `current` by one position in [`POOL_STEPS`], with no wrap-around: the
/// same rule [`ConfigModal::step`] uses for the tx-power rows.
fn step_pool(current: PoolArg, delta: i8) -> PoolArg {
    let index = POOL_STEPS.iter().position(|&pool| pool == current).unwrap_or(0);
    let index = if delta > 0 {
        (index + 1).min(POOL_STEPS.len() - 1)
    } else if delta < 0 {
        index.saturating_sub(1)
    } else {
        index
    };
    POOL_STEPS[index]
}

impl ConfigModal {
    /// Step the selected row by one: a tx-power row moves by one dBm, clamped
    /// to the range the file and the flags share; the pool row moves through
    /// [`POOL_STEPS`]. Both stop at their ends rather than wrapping.
    fn step(&mut self, delta: i8) {
        let value = match self.selected {
            Field::Fleet => &mut self.fleet_dbm,
            Field::Bridge => &mut self.bridge_dbm,
            Field::Pool => {
                self.pool = step_pool(self.pool, delta);
                return;
            }
        };
        *value = (*value + delta).clamp(*config::TX_POWER_DBM.start(), *config::TX_POWER_DBM.end());
    }
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
            KeyCode::Char('b') => self.toggle_ble(snapshot, commands),
            KeyCode::Char('c') => self.open_modal(snapshot),
            // Matched on the character rather than a shift modifier: crossterm
            // sends shift+r as `'R'`, not `'r'` with a modifier flag.
            KeyCode::Char('r') => self.clear_ring(snapshot, commands),
            KeyCode::Char('R') => self.clear_fleet_ring(snapshot, commands),
            // Anything else leaves the notice alone: a key bound to nothing must
            // not clear the one message saying why nothing happened.
            _ => {}
        }
    }

    /// Open the settings modal, seeded from the snapshot's current powers.
    ///
    /// Unreachable while a modal is already open: `view`'s main loop routes
    /// every key to [`Self::on_modal_key`] instead, once `self.modal` is
    /// `Some`, so `c` pressed again is simply not bound there.
    fn open_modal(&mut self, snapshot: &Snapshot) {
        self.notice = None;
        let fleet_dbm = snapshot.tx_power / 4;
        let bridge_dbm = snapshot.bridge_tx_power / 4;
        let pool = snapshot.pool.into();
        self.modal = Some(ConfigModal {
            selected: Field::Pool,
            fleet_dbm,
            bridge_dbm,
            pool,
            running_pool: pool,
        });
    }

    /// Keys while the settings modal is open. Nothing here reaches [`Self::on_key`]:
    /// `view`'s main loop chooses between them before either runs.
    fn on_modal_key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        commands: &mpsc::Sender<Command>,
    ) {
        let Some(modal) = self.modal.as_mut() else { return };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => modal.selected = modal.selected.next(),
            KeyCode::Up | KeyCode::Char('k') => modal.selected = modal.selected.prev(),
            KeyCode::Left | KeyCode::Char('h') => modal.step(-1),
            KeyCode::Right | KeyCode::Char('l') => modal.step(1),
            KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
            KeyCode::Enter => self.apply(snapshot, commands),
            // Same rule as `on_key`: a key bound to nothing leaves the notice
            // alone.
            _ => {}
        }
    }

    /// Send the modal's values to the engine, write them to `wartui.toml`,
    /// and close it.
    fn apply(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(modal) = self.modal.take() else { return };
        let command =
            Command::SetTxPower { nodes: modal.fleet_dbm * 4, bridge: modal.bridge_dbm * 4 };
        if commands.try_send(command).is_err() {
            self.say("the engine is not accepting commands".to_owned(), snapshot);
            return;
        }
        // Same rule as `toggle_ble`: the power arrives as an assignment, so a
        // fleet with no plan has nothing for it to arrive in yet.
        let when = if snapshot.plan.is_none() {
            "once the fleet is in a plan"
        } else {
            "on their next heartbeat"
        };
        let mut text = format!(
            "tx power: fleet {} dBm, bridge {} dBm — nodes take it {when}",
            modal.fleet_dbm, modal.bridge_dbm
        );
        text.push_str(&self.save_outcome(modal));
        self.say(text, snapshot);
    }

    /// What to append to the notice once `Enter` is pressed: where the save
    /// landed, or why it did not happen.
    ///
    /// The modal shows what is in force, and this writes every row of it to
    /// `wartui.toml`, replacing whatever the file held — a hand edit made
    /// while the modal was open included. The pool cannot change live, so a
    /// moved pool row exists only in the file: when the save does not happen,
    /// the notice says that pool was not kept.
    fn save_outcome(&self, modal: ConfigModal) -> String {
        let pool_moved = modal.pool != modal.running_pool;
        let lost = || {
            if pool_moved {
                format!("; pool {} was not kept", ChannelPool::from(modal.pool))
            } else {
                String::new()
            }
        };
        let Some(path) = self.settings.config_path.clone() else {
            return format!("; nowhere to save it — use --config{}", lost());
        };
        let saved = config::save(
            &path,
            &config::Config {
                pool: Some(modal.pool),
                tx_power: config::TxPower {
                    fleet: Some(modal.fleet_dbm),
                    bridge: Some(modal.bridge_dbm),
                },
            },
        );
        match saved {
            Err(error) => format!("; could not save: {error}{}", lost()),
            Ok(()) => {
                let mut outcome = format!("; saved to {}", path.display());
                if pool_moved {
                    outcome.push_str("; the pool takes effect after a restart");
                }
                outcome
            }
        }
    }

    /// Move the Bluetooth scan onto the selected node, or off it.
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
        let said = match commands
            .try_send(Command::AssignBle { mac: if holds { None } else { Some(target) } })
        {
            Ok(()) if holds => format!("{}: bluetooth off on its next heartbeat", mac(&target)),
            // The scan arrives as an assignment, so a fleet with no plan has nothing
            // for it to arrive in: above twenty nodes the planner refuses the whole
            // fleet, and "on its next heartbeat" would never come true. A surplus
            // node inside a plan is not this case — more nodes than the pool has
            // channels for leaves one undealt, and giving that one the scan deals it
            // the empty share the flag travels in, so it is told on its next
            // heartbeat like any other.
            Ok(()) if snapshot.plan.is_none() => {
                format!("{}: bluetooth, once it is in a plan", mac(&target))
            }
            Ok(()) => format!("{}: bluetooth on its next heartbeat", mac(&target)),
            Err(_) => "the engine is not accepting commands".to_owned(),
        };
        self.say(said, snapshot);
    }

    /// Clear the selected node's dedup ring on its next heartbeat.
    fn clear_ring(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        // Same refusal `toggle_ble` makes: the frame travels in the admin window,
        // and only a heartbeat opens one.
        if let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", mac(&target)), snapshot);
            return;
        }
        let said = match commands.try_send(Command::ClearRing { mac: Some(target) }) {
            Ok(()) => format!("{}: clearing its dedup ring on its next heartbeat", mac(&target)),
            Err(_) => "the engine is not accepting commands".to_owned(),
        };
        self.say(said, snapshot);
    }

    /// Clear every assignable node's dedup ring, each on its own next heartbeat.
    fn clear_fleet_ring(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        if snapshot.assignable == 0 {
            self.say("no node is heartbeating, so there is no ring to clear".to_owned(), snapshot);
            return;
        }
        let said = match commands.try_send(Command::ClearRing { mac: None }) {
            Ok(()) => format!(
                "clearing the dedup ring on {} nodes, each on its next heartbeat",
                snapshot.assignable
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
/// The count comes first: it always fits. The list after it is truncated
/// rather than wrapped, because a round-robin share is a dozen scattered
/// channels.
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
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) || is_ctrl_c(key)
}

/// Whether `key` is ctrl-c, which quits even while a modal is open — unlike `q`
/// and `Esc`, which only close it.
fn is_ctrl_c(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c'))
}

fn draw(frame: &mut Frame<'_>, snapshot: &Snapshot, ui: &mut Ui) {
    // Faults get their own lines, and only when there are any: sharing the footer
    // with the counters pushed the last of them off the terminal.
    let faults = fault_lines(&faults(snapshot), frame.area().width);
    let drops = u16::from(drop_line(&snapshot.counters).is_some());
    let footer_height = 1 + drops + u16::try_from(faults.len()).unwrap_or(u16::MAX);
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(footer_height),
    ])
    .areas(frame.area());

    draw_header(frame, header, snapshot);

    // Side by side when there is room for both, stacked when there is not: the
    // fleet table needs 74 columns before its state column starts clipping.
    let [fleet, stream] = if body.width >= 114 {
        Layout::horizontal([Constraint::Length(74), Constraint::Min(40)]).areas(body)
    } else {
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body)
    };
    draw_fleet(frame, fleet, snapshot, ui);
    draw_stream(frame, stream, snapshot);
    draw_footer(frame, footer, snapshot, ui, &faults);

    if let Some(modal) = &ui.modal {
        draw_settings_modal(frame, modal);
    }
}

/// The settings modal, centred over the live view behind it.
fn draw_settings_modal(frame: &mut Frame<'_>, modal: &ConfigModal) {
    let area = centered_rect(44, 9, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::bordered().title(" settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let styled = |selected: bool| {
        if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() }
    };
    let row = |label: &str, dbm: i8, selected: bool| {
        Line::from(Span::styled(format!("{label:<18}◂ {dbm:>2} dBm ▸"), styled(selected)))
    };
    let pool_row = Line::from(Span::styled(
        format!("{:<18}◂ {} ▸", "pool", ChannelPool::from(modal.pool)),
        styled(modal.selected == Field::Pool),
    ));
    let lines = vec![
        pool_row,
        row("fleet tx power", modal.fleet_dbm, modal.selected == Field::Fleet),
        row("bridge tx power", modal.bridge_dbm, modal.selected == Field::Bridge),
        Line::default(),
        Line::from("enter save · esc cancel"),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A box `width` by `height`, centred in `area` and clipped to it when it does
/// not fit.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let (link, border_color) = if let Some(bridge) = &snapshot.bridge {
        let state = if snapshot.link_up { "up" } else { "down" };
        (
            format!(
                "bridge {} ({:?}), fw v{} — link {state}",
                short_mac(&bridge.mac),
                bridge.chip,
                bridge.fw_version
            ),
            Color::Green,
        )
    } else {
        ("waiting for bridge".to_owned(), Color::Red)
    };

    // The reason a link is down belongs in the fault box, which is vertical and
    // wraps, rather than on this line, which shares its width with the bridge's
    // identity — appended here, the single most useful thing on screen is the
    // part a narrow terminal clips.
    let first = vec![Span::raw(link)];

    let mut second = vec![
        Span::raw(format!("pool {}  ", snapshot.pool)),
        planning(snapshot),
        Span::raw(format!("  session {}  ", elapsed(snapshot.now_ms - snapshot.started_at_ms),)),
    ];
    second.extend(position(snapshot));

    let body = vec![Line::from(first), Line::from(second)];
    frame.render_widget(
        Paragraph::new(body).block(
            Block::bordered().title(" wartui ").border_style(border_color).title_style(Color::Gray),
        ),
        area,
    );
}

/// The fleet planner status.
///
/// Displays how many nodes are healthy, or error states around planning the fleet.
fn planning(snapshot: &Snapshot) -> Span<'static> {
    let text = match snapshot.plan {
        Some(plan) => format!("{} of {}", plan.node_count(), snapshot.nodes.len()),
        None if snapshot.assignable > MAX_NODES => "too many nodes".to_owned(),
        None if snapshot.alive > 0 => "no node it can drive".to_owned(),
        None => "no nodes detected".to_owned(),
    };
    Span::styled(text, Style::new().fg(Color::Green))
}

/// Current GPS position, or a warning
fn position(snapshot: &Snapshot) -> Vec<Span<'static>> {
    let fix = if snapshot.position.is_located() {
        Span::default()
    } else {
        Span::styled(
            "pos none — these observations cannot be uploaded",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let mut spans = vec![fix];
    if let Some(gps) = &snapshot.gps
        && let Some(span) = receiver(gps, snapshot.position.source)
    {
        spans.push(span);
    }
    spans
}

/// What the receiver is doing, said only when it is not the thing answering.
///
/// A configured GPS that is not producing the rows' positions is this tier's one
/// failure, and it is otherwise silent — the rows keep coming, carrying the
/// position from before the drive started. The note stays up until it is fixed.
///
/// Returns nothing at all when the search found no receiver. Looking for one is the
/// default, so most captures that say nothing about a GPS are captures where there
/// was never going to be one, and a line reporting that on every one of them would
/// be a fault where there is no fault.
fn receiver(gps: &GpsView, source: PositionSource) -> Option<Span<'static>> {
    // Checked before the source: a fix stays usable for `max_age` after the puck
    // is unplugged, so for those seconds the rows really are coming from the GPS
    // and the port really is dead.
    if let GpsStatus::Failed(reason) = &gps.status {
        return Some(Span::styled(format!("  gps: {reason}"), Style::new().fg(Color::Yellow)));
    }
    if source == PositionSource::Gps {
        let sats = match gps.status {
            GpsStatus::Fixed { satellites: Some(n) } => format!(", {n} sats"),
            _ => String::new(),
        };
        return Some(Span::styled(format!("gps ok{sats}"), Style::new().fg(Color::Green)));
    }
    let text = match &gps.status {
        GpsStatus::Connecting => "  gps connecting".to_owned(),
        // Named, so the port is worth saying: the operator is watching the ladder
        // work through the rates their receiver might be at.
        GpsStatus::Scanning { port, baud } => format!("  gps scanning {port} @{baud}"),
        GpsStatus::Searching => "  gps searching".to_owned(),
        GpsStatus::Fixed { .. } => "  gps fix is stale".to_owned(),
        GpsStatus::Failed(_) => "  gps unreadable".to_owned(),
        GpsStatus::NoReceiver => return None,
    };
    Some(Span::styled(text, Style::new().fg(Color::Yellow)))
}

fn draw_fleet(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, ui: &mut Ui) {
    let header = Row::new(["node", "rssi", "beats", "obs", "lost", "last", "channels", "state"])
        .style(Style::new().add_modifier(Modifier::BOLD));

    let rows: Vec<Row<'_>> = snapshot
        .nodes
        .iter()
        .map(|node| {
            let (state, style) = node_state(node);
            Row::new(vec![
                Cell::from(node_label(node)),
                Cell::from(node.state.link_rssi.map_or_else(|| "—".to_owned(), |r| format!("{r}"))),
                Cell::from(node.state.heartbeats.to_string()),
                Cell::from(node.state.observations.to_string()),
                Cell::from(lost_cell(node)),
                Cell::from(ago(snapshot.now_ms - node.state.last_seen_ms)),
                Cell::from(channels_cell(node)),
                Cell::from(state).style(style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(CHANNELS_WIDTH),
        Constraint::Min(12),
    ];
    let title = format!(" fleet — {} of {} alive ", snapshot.alive, snapshot.nodes.len());
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut state = TableState::new()
        .with_offset(ui.fleet_offset)
        .with_selected((!snapshot.nodes.is_empty()).then_some(ui.selected));
    frame.render_stateful_widget(table, area, &mut state);
    ui.fleet_offset = state.offset();
}

/// How wide the channels column is, and therefore how much of the list fits.
const CHANNELS_WIDTH: u16 = 18;

/// What the node is scanning, and whether that is known, acknowledged, or
/// adopted.
///
/// Three states: pending (`dirty`, yellow `…`) is a share sent but not yet
/// acknowledged; acked-not-adopted (`!dirty`, confirmed, but the node's own
/// heartbeat has not yet said it holds it) is the confirmed set with a blue
/// `…`, since a MAC-layer ack is not proof the frame was actually taken —
/// [`NodeState::adopted`] is; adopted is the confirmed set plain.
///
/// An empty set is the Bluetooth node's and says so in words. The count-and-list
/// form would render it `0: none`, which reads as a fault rather than as the job it
/// is; a value of a different kind belongs in a different shape.
fn channels_cell(node: &NodeView) -> Span<'static> {
    let state = &node.state;
    if state.dirty
        && let Some(desired) = state.desired
    {
        if desired.channels.is_empty() {
            return Span::styled("bluetooth…", Style::new().fg(Color::Yellow));
        }
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
                let mut cell = channel_cell(confirmed.channels, usize::from(CHANNELS_WIDTH) - 1);
                if !cell.ends_with('…') {
                    cell.push('…');
                }
                Span::styled(cell, Style::new().fg(Color::Blue))
            }
        },
    )
}

/// Whole batches missing between this node and the host, or `—` before its
/// first one has arrived — which is not the same thing as zero: a node that
/// has never sent a batch has nothing to have lost yet.
///
/// Read off `observations` rather than `last_seq`: every batch carries at least one
/// record, and a reboot clears `last_seq` but not the losses counted before it.
fn lost_cell(node: &NodeView) -> String {
    if node.state.observations == 0 {
        "—".to_owned()
    } else {
        node.state.batches_lost.to_string()
    }
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

/// Why a node cannot be given an assignment, or `None` if it can.
///
/// The wording is the message the operator sees when `b` or `r` is refused, so it
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
            " q quit  ↑↓ select  b bluetooth  c settings  r clear  R clear fleet ",
            Style::new().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::raw(format!(
            "  frames {}  obs {}  beats {}  stored {}",
            c.frames, c.observations, c.heartbeats, snapshot.store.written
        )));
        if c.admin_sent > 0 {
            spans.push(Span::raw(format!("  admin {}/{}", c.admin_acked, c.admin_sent)));
        }
        if c.batches_lost > 0 {
            spans.push(Span::raw(format!("  lost {}", c.batches_lost)));
        }
        if c.duplicate_batches > 0 {
            spans.push(Span::raw(format!("  dup {}", c.duplicate_batches)));
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

    // A line of its own rather than a span of the totals, so a notice never hides
    // it; plain rather than yellow, since a full ring is expected in a dense area.
    if let Some(drops) = drop_line(&c) {
        lines.push(Line::from(drops));
    }

    // Already fitted to the width by `fault_lines`, so nothing here can be
    // clipped and no fault can push another one off the end.
    for fault in faults {
        lines.push(Line::from(Span::styled(fault.clone(), Style::new().fg(Color::Yellow))));
    }

    frame.render_widget(Paragraph::new(lines).dim(), area);
}

/// Sightings the fleet's nodes had no ring room for this session, or `None` while
/// there are none. Most are reported on the next dwell or scan, so this reads as
/// a measure of the rings against the area rather than as a fault.
fn drop_line(c: &Counters) -> Option<String> {
    if c.wifi_dropped == 0 && c.ble_dropped == 0 {
        return None;
    }
    let mut line = String::new();
    if c.wifi_dropped > 0 {
        line.push_str(&format!("  wifi drop {}", c.wifi_dropped));
    }
    if c.ble_dropped > 0 {
        line.push_str(&format!("  ble drop {}", c.ble_dropped));
    }
    Some(line)
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
    if c.admin_unadopted > 0 {
        faults.push(format!("{} assignments acknowledged but not adopted", c.admin_unadopted));
    }
    if c.peer_table_full > 0 {
        faults.push(format!("peer table full: more than {MAX_NODES} nodes"));
    }
    // The planner gives up rather than cut the pool among nodes that will never
    // hear the result; what is out there stays out there. Counted against
    // `assignable`, the list the planner is handed — a fleet of thirty of which
    // nineteen are ours partitions perfectly well.
    if snapshot.plan.is_none() && snapshot.assignable > MAX_NODES {
        faults.push(format!(
            "{} nodes alive: over the {MAX_NODES} wartui supports, so the fleet is no longer \
             being partitioned",
            snapshot.assignable
        ));
    }
    // Channels that went into no share. The planner leaving them out is right and
    // completely invisible, since what remains is an ordinary partition of the part
    // the fleet can reach — so the only account of them is here.
    //
    // Two causes, told apart the way `Plan::unreachable` says they can be: every
    // radio tunes 2.4 GHz, so a 2.4 GHz channel is in there only when no node is
    // sniffing at all.
    if let Some(plan) = snapshot.plan
        && !plan.unreachable().is_empty()
    {
        let no_sniffer = plan.unreachable().indices().any(|idx| !plan::is_five_ghz(idx));
        faults.push(if no_sniffer {
            format!(
                "every node in this fleet is scanning Bluetooth, so none of the {} channels of \
                 the {} pool is being swept",
                plan.unreachable().len(),
                snapshot.pool
            )
        } else {
            format!(
                "{} channels of the {} pool are 5 GHz and no node in this fleet is sniffing with \
                 a 5 GHz radio, so they are not being scanned",
                plan.unreachable().len(),
                snapshot.pool
            )
        });
    }
    if let Some(gps) = &snapshot.gps {
        if let GpsStatus::Failed(reason) = &gps.status {
            faults.push(format!("gps unreadable: {reason}"));
        }
        // Every line failing its checksum is a receiver talking at a rate nobody is
        // listening at. Worth saying only when the rate was the operator's: one the
        // ladder chose already produced valid sentences at that rate, so a flood of
        // rejects afterwards is a receiver that changed or a cable that is failing,
        // and `--gps-baud` is not the answer to either.
        if gps.counters.fixes == 0 && gps.counters.rejected > 20 {
            let rejected = gps.counters.rejected;
            faults.push(if gps.pinned_baud {
                format!("gps: {rejected} unreadable lines and no fix — wrong --gps-baud?")
            } else {
                match &gps.settled {
                    Some((port, baud)) => {
                        format!("gps: {rejected} unreadable lines and no fix from {port} @{baud}")
                    }
                    None => format!("gps: {rejected} unreadable lines and no fix"),
                }
            });
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
    use wartui_proto::air::{Capabilities, wire_epoch};
    use wartui_proto::link::Chip;
    use wartui_proto::plan::{
        ChannelPool, DEFAULT_TX_POWER_QUARTER_DBM, IndexRun, Job, Radio, plan, plan_for,
    };

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
        state.capabilities = Some(Capabilities::here(true));
        NodeView { state, alive: true, assignable }
    }

    /// A node heard only through its observations, which is an ordinary few
    /// seconds in the life of one that is about to be perfectly drivable: it
    /// reports what it found on a channel before it gets back to the control
    /// channel to heartbeat.
    fn unannounced(last: u8) -> NodeView {
        let mut view = node(last, 0, false);
        view.state.capabilities = None;
        view.state.last_heartbeat = None;
        view.alive = false;
        view
    }

    /// A node whose token says 2.4 GHz only: an ESP32-C6.
    fn narrowband(last: u8) -> NodeView {
        let mut view = node(last, 0, true);
        view.state.capabilities = Some(Capabilities::here(false));
        view
    }

    /// Heartbeating stopped and nothing else is wrong. The only one of the
    /// four refusals whose cause is on the node rather than in this host.
    fn stale_node(last: u8) -> NodeView {
        let mut view = node(last, 0, false);
        // Still being heard, and no longer heartbeating, which is what `stale` means.
        view.alive = false;
        view
    }

    /// Heartbeating perfectly well, with nowhere in the bridge's peer table to
    /// put it.
    fn refused(last: u8) -> NodeView {
        let mut view = node(last, 0, false);
        view.state.peer_refused = true;
        view.state.last_outcome = Some(AdminOutcome::Refused);
        view
    }

    /// Set `held_epoch` to match what `desired` (or, absent that, `confirmed`)
    /// carries, the way a node's own heartbeat would once it actually adopted
    /// the frame. [`NodeState::adopted`] is true after this.
    fn adopt(state: &mut NodeState) {
        let assignment = state.desired.or(state.confirmed).expect("something to adopt");
        state.held_epoch = Some(wire_epoch(assignment.counter));
    }

    /// A node that has been given channels, has acknowledged them, and whose
    /// own heartbeat has confirmed it holds them: fully settled.
    fn assigned(last: u8) -> NodeView {
        let mut view = node(last, 0, true);
        let assignment = Assignment {
            channels: ChannelSet::from_run(IndexRun::new(0, 0)),
            ble: false,
            tx_power: 8,
            counter: 9,
        };
        view.state.confirmed = Some(assignment);
        view.state.desired = Some(assignment);
        view.state.last_outcome = Some(AdminOutcome::Acked);
        view.state.last_latency_us = Some(4_200);
        adopt(&mut view.state);
        view
    }

    /// A node whose radio acknowledged its assignment, but whose own
    /// heartbeat has not yet said it holds it.
    fn acked_not_adopted(last: u8) -> NodeView {
        let mut view = assigned(last);
        view.state.held_epoch = None;
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
            channels: ChannelSet::from_run(IndexRun::new(14, 38)),
            ble: false,
            tx_power: 8,
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
                panel: None,
            }),
            link_up: true,
            link_error: None,
            // The built-in default, the same reason `tx_power`/`bridge_tx_power`
            // below are the built-in default: an unrelated test must not trip
            // the pool row's save rule just by calling `busy()`.
            pool: ChannelPool::All,
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
                admin_unadopted: 0,
                peer_table_full: 0,
                replans: 0,
                batches_lost: 0,
                duplicate_batches: 0,
                wifi_dropped: 0,
                ble_dropped: 0,
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
            tx_power: DEFAULT_TX_POWER_QUARTER_DBM,
            bridge_tx_power: DEFAULT_TX_POWER_QUARTER_DBM,
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

    /// Rendering must not panic at any size the terminal might be.
    ///
    /// A layout that divides by a zero-width area or indexes past a short one
    /// takes the whole capture down with it, in the alternate screen, where the
    /// backtrace is unreadable. These sizes are cheap insurance against that.
    #[test]
    fn view_renders_without_panicking_when_given_various_terminal_sizes() {
        for snapshot in [busy(), empty()] {
            for (width, height) in [(200, 50), (120, 30), (80, 24), (40, 10), (20, 5), (6, 3)] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("test backend");
                terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
            }
        }
    }

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
    fn view_shows_fleet_and_stream_side_by_side_when_terminal_is_wide() {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("C5 57:84"), "the fleet table");
        assert!(rendered.contains("AA:BB:CC:DD:EE"), "and the observation stream");
        assert!(rendered.contains("4 of 5 alive"));
        assert!(rendered.contains("no heartbeat"), "the one node nothing can be sent to yet");
    }

    #[test]
    fn view_displays_faults_only_when_they_have_occurred() {
        let mut clean = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        clean.draw(|frame| draw(frame, &empty(), &mut Ui::default())).expect("drawing");
        assert!(!clean.backend().to_string().contains("dropped"));

        let mut faulty = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        faulty.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        let rendered = faulty.backend().to_string();
        assert!(rendered.contains("store dropped 7"));
        assert!(rendered.contains("admin frames from another core"));
        assert!(rendered.contains("older firmware"));
        assert!(rendered.contains("vendor fleet"));
    }

    const BUSY: &str = "could not open /dev/cu.usbmodem101: Device or resource busy";

    #[test]
    fn fault_lines_wraps_long_message_when_fault_exceeds_terminal_width() {
        // Unwrapped, a reason this long would show its first clause on a narrow
        // terminal and nothing else, so `fault_lines` breaks it up instead.
        let fault = format!("link down: {BUSY}");
        let lines = fault_lines(std::slice::from_ref(&fault), 40);
        assert!(lines.len() > 1, "it does not fit on one line: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let rejoined: String = lines.iter().map(|l| l.trim_start()).collect();
        assert_eq!(rejoined, fault, "every character of it is on screen somewhere");
    }

    #[test]
    fn fault_lines_combines_messages_into_one_line_when_faults_fit_within_width() {
        let faults = ["store dropped 7".to_owned(), "unparsed 3".to_owned()];
        assert_eq!(fault_lines(&faults, 200), vec!["  store dropped 7  unparsed 3".to_owned()]);
    }

    #[test]
    fn fault_lines_appends_overflow_count_when_faults_exceed_footer_capacity() {
        let faults: Vec<String> = (0..12).map(|n| format!("fault number {n} of twelve")).collect();
        let lines = fault_lines(&faults, 40);
        assert_eq!(lines.len(), MAX_FAULT_LINES, "the fleet table keeps the rest of the screen");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let last = lines.last().expect("a line");
        assert!(last.contains("more"), "how many are not shown: {last}");
    }

    #[test]
    fn fault_lines_appends_truncated_indicator_when_single_fault_exceeds_box_capacity() {
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
    fn fault_lines_preserves_preceding_messages_when_overflow_notice_is_added() {
        // The notice takes a line of its own: taking its room out of the last fault
        // would leave a sentence that reads as complete and says something the OS
        // never said.
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
    fn fault_lines_keeps_overflow_notice_within_bounds_when_terminal_width_is_narrow() {
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
    fn view_displays_link_down_reason_when_terminal_is_narrow() {
        let mut snapshot = empty();
        snapshot.link_error = Some(BUSY.to_owned());
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("link down"), "{screen}");
        // The tail of the reason is the part that names what to do about it.
        assert!(screen.contains("busy"), "{screen}");
    }

    #[test]
    fn view_displays_link_error_details_when_link_is_down() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &empty(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("waiting for bridge"));
        assert!(rendered.contains("no bridge found"));
    }

    #[test]
    fn view_displays_pos_none_indicator_when_capture_lacks_position_fix() {
        let mut positioned = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        positioned.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        assert!(!positioned.backend().to_string().contains("pos none"));

        let mut without = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        without.draw(|frame| draw(frame, &empty(), &mut Ui::default())).expect("drawing");
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
        snapshot.gps = Some(GpsView {
            status,
            counters,
            last_fix_ms: Some(EPOCH_MS),
            settled: Some(("/dev/ttyACM1".to_owned(), 9600)),
            pinned_baud: false,
        });
        snapshot
    }

    /// The same, for a receiver the operator named a rate for.
    fn with_pinned_gps(status: GpsStatus, counters: GpsCounters) -> Snapshot {
        let mut snapshot = with_gps(status, counters, PositionSource::Static);
        if let Some(gps) = snapshot.gps.as_mut() {
            gps.pinned_baud = true;
        }
        snapshot
    }

    fn rendered(snapshot: &Snapshot) -> String {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, &mut Ui::default())).expect("drawing");
        terminal.backend().to_string()
    }

    #[test]
    fn header_displays_pool_name_when_rendering_active_channel_pool() {
        // The header is the only place the pool is named on screen, and it
        // spells each one the way the manual does.
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Us;
        assert!(rendered(&snapshot).contains("pool US"));

        snapshot.pool = ChannelPool::Eu;
        assert!(rendered(&snapshot).contains("pool EU"));

        snapshot.pool = ChannelPool::All;
        assert!(rendered(&snapshot).contains("pool All"));
    }

    #[test]
    fn view_explains_gps_status_when_receiver_is_searching_or_stale() {
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
    fn view_displays_satellite_count_when_gps_fix_is_active() {
        let fixed = with_gps(
            GpsStatus::Fixed { satellites: Some(8) },
            GpsCounters { sentences: 400, fixes: 200, rejected: 1 },
            PositionSource::Gps,
        );
        let screen = rendered(&fixed);
        assert!(!screen.contains("pos none"), "{screen}");
        assert!(screen.contains("gps ok, 8 sats"), "{screen}");
    }

    #[test]
    fn view_omits_gps_status_when_no_receiver_is_present() {
        // Most captures are static, and a permanent "gps: none" would be noise.
        assert!(!rendered(&busy()).contains("gps"));
    }

    #[test]
    fn view_displays_failure_message_when_receiver_disconnects_mid_capture() {
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
    fn view_reports_gps_fault_when_receiver_is_unreadable_or_baud_rate_is_wrong() {
        let failed = with_gps(
            GpsStatus::Failed("/dev/cu.gps: No such file or directory".to_owned()),
            GpsCounters::default(),
            PositionSource::Static,
        );
        assert!(rendered(&failed).contains("gps unreadable"));

        // A rate the operator chose is the one that `--gps-baud` can answer for.
        let mistuned = with_pinned_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
        );
        assert!(rendered(&mistuned).contains("wrong --gps-baud"));
    }

    #[test]
    fn view_reports_unreadable_lines_without_blaming_baud_flag_when_baud_rate_was_auto_detected() {
        // The ladder settles on a rate by getting valid sentences out of it, so
        // unreadable lines afterwards are a receiver that changed or a cable that is
        // failing. Sending the operator to `--gps-baud` would send them nowhere.
        let noisy = with_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
            PositionSource::Static,
        );
        let drawn = rendered(&noisy);
        assert!(!drawn.contains("--gps-baud"), "{drawn}");
        assert!(drawn.contains("300 unreadable lines"), "{drawn}");
    }

    #[test]
    fn view_suppresses_gps_fault_line_when_no_receiver_was_found() {
        // Searching is the default, so most captures with no receiver are captures
        // where there was never going to be one. Saying so on every one of them
        // would put a fault on the view where there is no fault.
        let none = with_gps(GpsStatus::NoReceiver, GpsCounters::default(), PositionSource::Static);
        let drawn = rendered(&none);
        assert!(!drawn.contains("gps"), "{drawn}");
        // A static position still keeps the unpositioned warning off screen.
        assert!(!drawn.contains("pos none"), "{drawn}");
    }

    #[test]
    fn view_displays_port_and_baud_rate_when_gps_ladder_is_scanning() {
        let scanning = with_gps(
            GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 38_400 },
            GpsCounters::default(),
            PositionSource::Static,
        );
        assert!(rendered(&scanning).contains("gps scanning /dev/ttyUSB0 @38400"));
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
    fn view_reports_bridge_drops_only_when_drops_occur_during_current_capture() {
        let screen = rendered(&with_bridge_drops(5));
        assert!(screen.contains("bridge dropped 5"), "{screen}");

        // `busy()`'s bridge dropped 1300 frames before this host attached.
        let screen = rendered(&busy());
        assert!(!screen.contains("bridge dropped"), "{screen}");
    }

    /// The footer's running-totals line, identified by `frames` rather than
    /// position: the fleet table's `lost` column header would otherwise make
    /// a plain substring search for "lost" match every screen.
    fn totals_line(screen: &str) -> String {
        screen.lines().find(|line| line.contains("frames")).unwrap_or_default().to_owned()
    }

    #[test]
    fn view_shows_lost_total_when_batches_lost_is_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.batches_lost = 7;
        let screen = rendered(&snapshot);
        assert!(totals_line(&screen).contains("lost 7"), "{screen}");

        // `busy()` itself has nothing lost, the same rule `admin` follows.
        let screen = rendered(&busy());
        assert!(!totals_line(&screen).contains("lost"), "{screen}");
    }

    #[test]
    fn view_shows_dup_total_when_duplicate_batches_is_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.duplicate_batches = 3;
        let screen = rendered(&snapshot);
        assert!(totals_line(&screen).contains("dup 3"), "{screen}");

        // `busy()` itself has nothing duplicated, the same rule `lost` follows.
        let screen = rendered(&busy());
        assert!(!totals_line(&screen).contains("dup"), "{screen}");
    }

    #[test]
    fn footer_hides_drop_line_when_both_counts_zero() {
        let screen = rendered(&busy());
        assert!(!screen.contains("wifi drop"), "{screen}");
        assert!(!screen.contains("ble drop"), "{screen}");
    }

    #[test]
    fn footer_displays_drop_line_when_wifi_count_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.wifi_dropped = 12;
        let screen = rendered(&snapshot);
        let line = screen.lines().find(|l| l.contains("wifi drop")).expect("a drop line");
        assert!(line.contains("wifi drop 12"), "{screen}");
        // A kind with nothing dropped is left out rather than shown as 0.
        assert!(!line.contains("ble drop"), "{screen}");
        assert!(!line.contains("frames"), "a line of its own, not a span of the totals");

        snapshot.counters.ble_dropped = 4;
        assert!(rendered(&snapshot).contains("wifi drop 12  ble drop 4"));
    }

    #[test]
    fn footer_keeps_drop_line_when_notice_shown() {
        // The notice takes the totals line; the drops are on their own line so
        // they stay.
        let mut snapshot = busy();
        snapshot.counters.wifi_dropped = 12;
        snapshot.counters.ble_dropped = 4;
        let mut ui = Ui { notice: Some(("a notice".to_owned(), snapshot.now_ms)), ..Ui::default() };
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("a notice"), "{screen}");
        assert!(!screen.contains("stored 800"), "the notice replaced the totals: {screen}");
        assert!(screen.contains("wifi drop 12  ble drop 4"), "{screen}");
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
    fn view_reports_bridge_reboot_when_reset_cause_is_watchdog() {
        let screen = rendered(&with_restart(ResetCause::Watchdog, LoopPhase::Transmit));
        assert!(screen.contains("watchdog"), "{screen}");

        // Every capture starts with a bridge that was powered on. Matched on the
        // full phrase: nodes have their own `rebooted` column.
        let screen = rendered(&with_restart(ResetCause::PowerOn, LoopPhase::Unknown));
        assert!(!screen.contains("bridge rebooted"), "{screen}");
    }

    #[test]
    fn view_reports_usb_tx_stall_when_bridge_reboots_due_to_tx_stalled() {
        let screen = rendered(&with_restart(ResetCause::Software, LoopPhase::TxStalled));
        assert!(screen.contains("USB transmit had stalled"), "{screen}");
    }

    #[test]
    fn quits_returns_true_when_receiving_q_esc_or_ctrl_c() {
        assert!(quits(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
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
        // Indices into SCAN_CHANNELS are an artefact of the wire format.
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
        // The "asked for, not yet acknowledged" marker and the "more than
        // fitted" marker are the same character, and two in a row is not a
        // different meaning, just a worse-looking cell.
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
        // A node that departs the plan (`replan`'s departed loop, or
        // `PeerTableFull`) keeps `confirmed` while `desired` clears.
        // `adopted` has to read against what it still holds, not nothing.
        let mut view = assigned(0x11);
        view.state.desired = None;
        assert!(view.state.adopted(), "confirmed is still held, and its epoch still matches");
        let cell = channels_cell(&view);
        assert!(!cell.content.ends_with('…'), "adopted, not stuck waiting");
        assert_eq!(cell.style.fg, None, "plain, not coloured");
    }

    #[test]
    fn ui_displays_specific_refusal_notice_when_toggling_ble_on_unassignable_node() {
        // Three faults all end in a refused `b` and the operator's next move
        // differs for each, so one wording for all three misdirects two.
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
            ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
            assert!(rx.try_recv().is_err(), "row {row} queued something");
            let notice = ui.notice(snapshot.now_ms).expect("a reason");
            assert!(notice.contains(want), "row {row}: wanted {want:?}, got {notice}");
        }
    }

    #[test]
    fn view_warns_no_drivable_nodes_when_all_nodes_are_refused_peer_slots() {
        // Every node heartbeating, none reachable — a fleet that has outgrown
        // the bridge's twenty peer slots, so there is nothing to partition.
        let mut snapshot = busy();
        snapshot.plan = None;
        snapshot.nodes = vec![refused(0x21), refused(0x22), refused(0x23)];
        snapshot.alive = 3;
        snapshot.assignable = 0;

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("no node it can drive"), "got {rendered}");
        assert!(!rendered.contains("nothing heartbeating"), "they are all heartbeating");
    }

    #[test]
    fn ui_displays_plan_dependency_notice_when_assigning_ble_on_fleet_without_plan() {
        // A node the planner dealt nothing is still in the plan, so giving it the scan
        // deals it the empty share the flag travels in and it hears on its next
        // heartbeat like any other. The wait is for a fleet with no plan at all, where
        // `replan` sends nothing to anybody — and that node may well be holding an
        // assignment from when the fleet was smaller, so what it holds says nothing
        // about whether the flag can reach it.
        let mut snapshot = busy();
        snapshot.plan = plan_for(ChannelPool::Us, &[Job::Wifi(Radio::TwoPointFour); 12]);
        let surplus = snapshot
            .nodes
            .iter()
            .position(|n| n.assignable && n.state.desired.is_none() && n.state.confirmed.is_none())
            .expect("a node with no share of its own");

        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui { selected: surplus, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert!(rx.try_recv().is_ok(), "the command goes out either way");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("on its next heartbeat"), "got {notice}");

        let mut planless = snapshot.clone();
        planless.plan = None;
        let holder = planless
            .nodes
            .iter()
            .position(|n| n.assignable && n.state.confirmed.is_some())
            .expect("a node still holding what it was given");
        let mut ui = Ui { selected: holder, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &planless, &tx);
        let notice = ui.notice(planless.now_ms).expect("a notice");
        assert!(notice.contains("once it is in a plan"), "got {notice}");
    }

    #[test]
    fn view_warns_missing_five_ghz_coverage_when_fleet_has_no_five_ghz_radios() {
        let mut snapshot = busy();
        snapshot.plan = plan_for(ChannelPool::Us, &[Job::Wifi(Radio::TwoPointFour); 2]);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("no node in this fleet is sniffing with a 5 GHz radio"),
            "got {rendered}"
        );
    }

    #[test]
    fn view_warns_no_wifi_sniffing_when_all_nodes_scan_bluetooth() {
        let mut snapshot = busy();
        snapshot.plan = plan_for(ChannelPool::Us, &[Job::Bluetooth]);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("every node in this fleet is scanning Bluetooth"),
            "got {rendered}"
        );
    }

    #[test]
    fn channels_cell_displays_bluetooth_when_node_is_assigned_bluetooth_scan() {
        // `0: none` would read as a fault rather than as the job it is.
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
        // The honest case: observations have arrived and no heartbeat yet, so
        // there has been no window to send anything through.
        let view = unannounced(0x21);
        let (label, _) = node_state(&view);
        assert_eq!(label, "no heartbeat");
        assert!(view.state.observations > 0, "and it is not silent");
    }

    #[test]
    fn view_displays_planner_status_when_rendering_header() {
        let mut empty = busy();
        empty.nodes.clear();
        empty.alive = 0;
        empty.assignable = 0;
        let mut waiting = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        waiting.draw(|frame| draw(frame, &empty, &mut Ui::default())).expect("drawing");
        assert!(waiting.backend().to_string().contains("no nodes detected"));

        let mut snapshot = busy();
        snapshot.plan = plan(ChannelPool::Us, 4);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        assert!(terminal.backend().to_string().contains("4 of 5"));

        // A lone node holds the whole pool, and the header has nothing extra to
        // say about it.
        let mut lone = busy();
        lone.plan = plan(ChannelPool::Us, 1);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &lone, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("1 of 5"));
        assert!(!rendered.contains("rotating"));
    }

    #[test]
    fn ui_toggles_ble_assignment_when_b_key_is_pressed() {
        let snapshot = busy();
        let node = snapshot.nodes[0].state.mac;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
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
    fn ui_sends_clear_ring_when_r_key_is_pressed() {
        let snapshot = busy();
        let node = snapshot.nodes[0].state.mac;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::ClearRing { mac: Some(node) });
        assert!(
            ui.notice(snapshot.now_ms).expect("a notice").contains("clearing its dedup ring"),
            "got {:?}",
            ui.notice(snapshot.now_ms)
        );
    }

    #[test]
    fn ui_sends_fleet_clear_ring_when_shift_r_key_is_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        // Crossterm sends shift+r as `'R'`, not `'r'` with a shift modifier.
        ui.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::ClearRing { mac: None });
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("clearing the dedup ring on"), "got {notice}");
    }

    #[test]
    fn ui_refuses_clear_ring_when_node_has_not_heartbeated_yet() {
        let mut snapshot = busy();
        snapshot.nodes.push(unannounced(0x21));
        snapshot.nodes.sort_by_key(|n| n.state.mac);
        let row = snapshot
            .nodes
            .iter()
            .position(|n| n.state.mac[5] == 0x21)
            .expect("the unannounced node");

        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui { selected: row, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &snapshot, &tx);
        assert!(rx.try_recv().is_err(), "nothing was queued");
        let notice = ui.notice(snapshot.now_ms).expect("a reason");
        assert!(notice.contains("heartbeated yet"), "got {notice}");
    }

    #[test]
    fn ui_sends_nothing_when_fleet_clear_requested_with_no_assignable_nodes() {
        let mut snapshot = busy();
        snapshot.assignable = 0;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), &snapshot, &tx);
        assert!(rx.try_recv().is_err(), "nothing was queued");
        let notice = ui.notice(snapshot.now_ms).expect("a reason");
        assert!(notice.contains("no node is heartbeating"), "got {notice}");
    }

    #[test]
    fn fleet_table_shows_bluetooth_in_channel_column_when_node_holds_or_awaits_ble() {
        let mut snapshot = busy();
        // The stream pane names nodes by the same octets, on the same lines.
        snapshot.tail = Vec::new();
        let mac = snapshot.nodes[0].state.mac;

        // Adopted: confirmed and desired agree on the Bluetooth assignment,
        // and the node's own heartbeat has said it holds it.
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
        assert!(screen.contains("lost"), "the column header: {screen}");
        assert!(screen.contains("42"), "the node with missed batches: {screen}");
    }

    #[test]
    fn view_warns_partitioning_has_stopped_when_fleet_exceeds_max_nodes() {
        let mut snapshot = busy();
        snapshot.plan = None;
        snapshot.assignable = MAX_NODES + 1;
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        assert!(terminal.backend().to_string().contains("no longer"));
    }

    #[test]
    fn ui_clamps_cursor_to_valid_row_when_navigating_or_fleet_size_changes() {
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

    #[test]
    fn fleet_table_scrolls_to_selected_node_when_nodes_exceed_visible_rows() {
        let mut snapshot = busy();
        snapshot.nodes = (0..15u8).map(|last| node(last, 0, true)).collect();
        snapshot.alive = 15;
        snapshot.assignable = 15;
        snapshot.tail = Vec::new();

        // Wide enough for the side-by-side layout, short enough that the fleet
        // box has only a handful of data rows once its header and borders are
        // paid for — fewer than the fifteen nodes above.
        let mut terminal = Terminal::new(TestBackend::new(200, 16)).expect("test backend");
        let mut ui = Ui { selected: 14, ..Default::default() };
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("57:0E"), "the selected node scrolled into view: {rendered}");
        assert!(!rendered.contains("57:00"), "the first node should have scrolled off: {rendered}");

        // Moving the cursor up one row, still well inside the window, must not
        // move the window: the offset persisted in `Ui` is what keeps scrolling
        // minimal rather than recomputed from row 0 every frame.
        let offset_at_bottom = ui.fleet_offset;
        ui.selected -= 1;
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        assert_eq!(
            ui.fleet_offset, offset_at_bottom,
            "the window moved for a cursor move inside it"
        );
    }

    #[test]
    fn ui_opens_modal_with_snapshot_values_when_c_key_is_pressed() {
        let mut snapshot = busy();
        snapshot.tx_power = 40; // 10 dBm
        snapshot.bridge_tx_power = 60; // 15 dBm
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let modal = ui.modal.expect("the modal opened");
        assert_eq!(modal.selected, Field::Pool);
        assert_eq!(modal.fleet_dbm, 10);
        assert_eq!(modal.bridge_dbm, 15);
        assert!(rx.try_recv().is_err(), "opening the modal sends nothing");
    }

    #[test]
    fn ui_seeds_pool_row_from_snapshot_pool_when_modal_opened() {
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Eu;
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let modal = ui.modal.expect("the modal opened");
        assert_eq!(modal.pool, PoolArg::Eu);
        assert_eq!(modal.running_pool, PoolArg::Eu);
    }

    #[test]
    fn ui_reaches_bridge_row_from_pool_when_j_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal.expect("still open").selected, Field::Fleet);

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(ui.modal.expect("still open").selected, Field::Bridge);
    }

    #[test]
    fn ui_steps_pool_and_stops_at_ends_when_h_or_l_pressed() {
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::All;
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal.expect("still open").selected, Field::Pool, "opens on the pool row");

        for _ in 0..3 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        }
        assert_eq!(ui.modal.expect("still open").pool, PoolArg::All, "stops at the start");

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal.expect("still open").pool, PoolArg::Eu, "steps forward one at a time");

        for _ in 0..5 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        }
        assert_eq!(ui.modal.expect("still open").pool, PoolArg::Us, "stops at the end");
    }

    #[test]
    fn ui_closes_modal_without_sending_a_command_when_esc_or_q_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        for closer in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut ui = Ui::default();
            ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
            assert!(ui.modal.is_some(), "the modal is open");

            ui.on_modal_key(KeyEvent::new(closer, KeyModifiers::NONE), &snapshot, &tx);

            assert!(ui.modal.is_none(), "{closer:?} closes it");
            assert!(rx.try_recv().is_err(), "{closer:?} cancels rather than applies");
        }
    }

    #[test]
    fn ui_modal_clamps_step_to_two_and_twenty_dbm_when_stepping_past_the_range() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);

        for _ in 0..30 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        }
        assert_eq!(ui.modal.expect("still open").fleet_dbm, 2, "stops at the floor");

        for _ in 0..40 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        }
        assert_eq!(ui.modal.expect("still open").fleet_dbm, 20, "stops at the ceiling");
    }

    #[test]
    fn ui_sends_set_tx_power_in_quarter_dbm_when_enter_pressed() {
        let mut snapshot = busy();
        snapshot.tx_power = 40;
        snapshot.bridge_tx_power = 60;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetTxPower { nodes: 44, bridge: 60 });
        assert!(ui.modal.is_none(), "applying closes the modal");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("fleet 11 dBm, bridge 15 dBm"), "{notice}");
    }

    #[test]
    fn ui_says_nodes_take_it_once_fleet_is_in_a_plan_when_no_plan_exists() {
        // Same rule as `toggle_ble`: the power arrives as an assignment, so it
        // has nowhere to arrive until the fleet is in a plan.
        let snapshot = busy(); // `busy()`'s plan is `None`.
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("once the fleet is in a plan"), "{notice}");
    }

    #[test]
    fn ui_says_nodes_take_it_on_next_heartbeat_when_plan_exists() {
        let mut snapshot = busy();
        snapshot.plan = plan(ChannelPool::Us, 4);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("on their next heartbeat"), "{notice}");
    }

    #[test]
    fn ui_sends_only_tx_power_and_warns_of_restart_when_enter_pressed_with_pool_changed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::All;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui =
            Ui { settings: Settings { config_path: Some(target.clone()) }, ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        // Only the tx-power command goes out: the pool can't change live.
        assert_eq!(rx.try_recv().unwrap(), Command::SetTxPower { nodes: 8, bridge: 8 });
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("the pool takes effect after a restart"), "{notice}");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.pool, Some(PoolArg::Eu));
    }

    #[test]
    fn ui_saves_all_rows_and_names_file_in_notice_when_enter_pressed_with_nothing_moved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Eu;
        snapshot.tx_power = 40; // 10 dBm
        snapshot.bridge_tx_power = 60; // 15 dBm
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui =
            Ui { settings: Settings { config_path: Some(target.clone()) }, ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_ok(), "the live change still goes out");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains(&target.display().to_string()), "{notice}");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.pool, Some(PoolArg::Eu));
        assert_eq!(saved.tx_power.fleet, Some(10));
        assert_eq!(saved.tx_power.bridge, Some(15));
    }

    #[test]
    fn ui_overwrites_hand_edit_made_mid_run_when_enter_pressed() {
        // Enter saves what the modal shows, not what the file holds: a hand
        // edit made while the modal is open is overwritten just the same as
        // whatever was there when it opened.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "pool = \"us\"\n[tx-power]\nfleet = 5\nbridge = 6\n").unwrap();
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Eu;
        snapshot.tx_power = 40; // 10 dBm
        snapshot.bridge_tx_power = 60; // 15 dBm
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui { settings: Settings { config_path: Some(target.clone()) }, ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        // Hand-edited while the modal is open: a different pool, fleet and bridge.
        std::fs::write(&target, "pool = \"all\"\n[tx-power]\nfleet = 2\nbridge = 3\n").unwrap();
        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.pool, Some(PoolArg::Eu), "the modal's value wins over the hand edit");
        assert_eq!(saved.tx_power.fleet, Some(10));
        assert_eq!(saved.tx_power.bridge, Some(15));
    }

    #[test]
    fn ui_leaves_file_byte_identical_when_enter_pressed_again_with_the_same_values() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui { settings: Settings { config_path: Some(target.clone()) }, ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);
        let after_first = std::fs::read_to_string(&target).expect("written by the first save");

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let after_second = std::fs::read_to_string(&target).expect("still there");
        assert_eq!(after_first, after_second, "the second save touched nothing");
    }

    #[test]
    fn ui_saves_snapshot_values_when_they_came_from_a_flag() {
        // `--pool us` and `--node-tx-power` land in the snapshot the same way
        // any other running value does, and Enter saves whatever is on screen
        // regardless of where it came from.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "pool = \"eu\"\n[tx-power]\nfleet = 5\n").unwrap();
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Us;
        snapshot.tx_power = 48; // 12 dBm
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui { settings: Settings { config_path: Some(target.clone()) }, ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.pool, Some(PoolArg::Us));
        assert_eq!(saved.tx_power.fleet, Some(12));
    }

    #[test]
    fn ui_omits_restart_notice_when_enter_pressed_with_pool_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui { settings: Settings { config_path: Some(target.clone()) }, ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        // Only the fleet row moves; the pool row is left alone.
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("restart"), "{notice}");
    }

    #[test]
    fn ui_reports_nowhere_to_save_when_no_config_path_is_set() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui { settings: Settings::default(), ..Ui::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("use --config"), "{notice}");
        assert!(notice.contains("pool EU was not kept"), "the moved pool row: {notice}");
    }

    #[test]
    fn ui_reports_pool_not_kept_when_save_fails_with_pool_moved() {
        // A regular file where the config's directory should be, so the save
        // cannot create it.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui {
            settings: Settings { config_path: Some(blocker.join("wartui.toml")) },
            ..Ui::default()
        };
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("could not save"), "{notice}");
        assert!(notice.contains("pool EU was not kept"), "{notice}");
    }

    #[test]
    fn ui_omits_pool_not_kept_when_save_is_impossible_with_pool_unchanged() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("not kept"), "{notice}");
    }

    #[test]
    fn draw_renders_settings_modal_over_the_live_view_when_open() {
        let snapshot = busy();
        let mut ui = Ui::default();
        let (tx, _rx) = mpsc::channel(4);
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("settings"), "{rendered}");
        assert!(rendered.contains("fleet tx power"), "{rendered}");
        assert!(rendered.contains("bridge tx power"), "{rendered}");
        assert!(rendered.contains("pool"), "{rendered}");
        // The view behind it is still live, not blanked out.
        assert!(rendered.contains("C5 57:84"), "{rendered}");
    }
}
