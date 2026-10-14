//! The settings modal shows what is in force, not what `wartui.toml` holds. The two
//! differ after a hand edit or a failed save, and the operator is changing the running
//! capture. `Enter` puts every row in force, then writes every row to the file.
//!
//! `Enter` on the excluded-SSIDs page applies and saves only the list, then returns to
//! the rows. The operator opened the page to change the list, not the rows above it, so
//! their pending edits stay pending for the main `Enter` or `Esc`. `Esc` on the page drops
//! only the page's edits, so backing out of a list never costs the rows above it either.

use std::fmt::Display;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use tokio::sync::mpsc;
use wartui_bridge::remember::BridgeMemory;
use wartui_core::engine::{Command, Snapshot};
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelPool;

use super::UploadTarget;
use super::centered_rect;
use super::ui::ENGINE_BUSY;
use crate::config;
use crate::run::PoolArg;

/// What saving settings needs that a [`Snapshot`] lacks.
///
/// `run` builds it once at startup. The snapshot cannot supply it: the config path
/// depends on how the process started, and a failed save leaves the file out of step
/// with what is running.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Where `wartui.toml` is written. `None` when there is no default location and no
    /// `--config`.
    pub config_path: Option<PathBuf>,
    /// The file as last loaded or saved. `b` rewrites only the Bluetooth node from this
    /// copy, so it never puts the running pool or powers into the file.
    pub saved: config::Config,
    /// Whether `run` remembers the bridge. Shared with the transport, so the
    /// remember-bridge switch takes effect on the next reconnect.
    pub bridge_memory: BridgeMemory,
    /// Where `u` uploads to.
    pub upload: UploadTarget,
}

/// One row of the settings modal. A new setting adds a variant here and a row in
/// [`draw_settings_modal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Pool,
    Fleet,
    Bridge,
    RememberBle,
    RememberBridge,
    ExcludedSsids,
    WdgwarsKey,
}

/// Every row of the settings modal, top to bottom, in the order `next` and `prev` walk.
const FIELDS: [Field; 7] = [
    Field::Pool,
    Field::Fleet,
    Field::Bridge,
    Field::RememberBle,
    Field::RememberBridge,
    Field::ExcludedSsids,
    Field::WdgwarsKey,
];

impl Field {
    /// The row below. The last row stays put rather than wrapping.
    fn next(self) -> Self {
        let index = FIELDS.iter().position(|&field| field == self).unwrap_or(0);
        FIELDS[(index + 1).min(FIELDS.len() - 1)]
    }

    /// The row above. The first row stays put rather than wrapping.
    fn prev(self) -> Self {
        let index = FIELDS.iter().position(|&field| field == self).unwrap_or(0);
        FIELDS[index.saturating_sub(1)]
    }
}

/// The settings modal's state while open. Seeded from the snapshot at opening, then
/// edited independently of it.
#[derive(Debug, Clone)]
pub(super) struct ConfigModal {
    selected: Field,
    /// Whole dBm, the unit the operator sees and `config::TX_POWER_DBM` bounds.
    fleet_dbm: i8,
    /// Whole dBm, the bridge's effective power. Always a number: the row shows what is
    /// in force, not whether anything set it.
    bridge_dbm: i8,
    /// The pool row's current value, edited by `step`.
    pool: PoolArg,
    /// Whether `b` remembers the node it gives the scan to.
    remember_ble: bool,
    /// Whether `run` remembers the bridge it connected to.
    remember_bridge: bool,
    /// The WDGWars API key, typed or pasted. Empty means not set.
    wdgwars_key: String,
    /// The SSIDs never captured, edited on [`Page::Excluded`].
    excluded: Vec<String>,
    /// Which page of the modal is showing.
    page: Page,
}

/// The modal's pages. The main rows, or the list one row opens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    Main,
    Excluded {
        /// The selected entry of [`ConfigModal::excluded`].
        cursor: usize,
        /// The SSID being typed after `a`, or `None` while moving through the list.
        input: Option<String>,
        /// Why the last `Enter` in the input refused it.
        error: Option<&'static str>,
        /// The list as the page opened, restored when `Esc` backs out.
        before: Vec<String>,
    },
}

/// `All → Eu → Us`, the order the pool row steps through.
const POOL_STEPS: [PoolArg; 3] = [PoolArg::All, PoolArg::Eu, PoolArg::Us];

/// Step `current` one place through [`POOL_STEPS`], wrapping past either end like the
/// tx-power rows in [`ConfigModal::step`].
fn step_pool(current: PoolArg, delta: i8) -> PoolArg {
    let len = POOL_STEPS.len();
    let index = POOL_STEPS.iter().position(|&pool| pool == current).unwrap_or(0);
    let index = match delta.signum() {
        1 => (index + 1) % len,
        -1 => (index + len - 1) % len,
        _ => index,
    };
    POOL_STEPS[index]
}

impl ConfigModal {
    /// Step the selected row by one. Every row wraps past its ends.
    ///
    /// | Row | Step |
    /// |---|---|
    /// | tx power | 1 dBm, within `config::TX_POWER_DBM` |
    /// | pool | one place in [`POOL_STEPS`] |
    /// | remember | flips |
    /// | excluded ssids | none; [`Self::on_key`] opens its page |
    /// | key | none; [`Self::on_key`] types into it |
    fn step(&mut self, delta: i8) {
        let value = match self.selected {
            Field::Fleet => &mut self.fleet_dbm,
            Field::Bridge => &mut self.bridge_dbm,
            Field::Pool => {
                self.pool = step_pool(self.pool, delta);
                return;
            }
            Field::RememberBle => {
                if delta != 0 {
                    self.remember_ble = !self.remember_ble;
                }
                return;
            }
            Field::RememberBridge => {
                if delta != 0 {
                    self.remember_bridge = !self.remember_bridge;
                }
                return;
            }
            // A text row, and a row that opens a page. `on_key` handles both.
            Field::WdgwarsKey | Field::ExcludedSsids => return,
        };
        // In i16, so stepping past either end of i8 cannot overflow.
        let start = i16::from(*config::TX_POWER_DBM.start());
        let len = i16::from(*config::TX_POWER_DBM.end()) - start + 1;
        let stepped = start + (i16::from(*value) + i16::from(delta) - start).rem_euclid(len);
        *value = i8::try_from(stepped).expect("wraps within TX_POWER_DBM, which is i8");
    }
}

/// What a key did to the open modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ModalAction {
    /// Still open: a row moved, stepped or was typed into, or the key was unbound.
    Stay,
    /// `Esc` or `q`: close without applying.
    Close,
    /// `Enter`: apply every row, save, and close.
    Apply,
    /// `Enter` on the excluded-SSIDs page: apply and save only the list, then return to
    /// the rows.
    SaveExcluded,
}

impl ConfigModal {
    /// Open the modal, seeded from what is in force.
    pub(super) fn open(snapshot: &Snapshot, settings: &Settings) -> Self {
        Self {
            selected: Field::Pool,
            fleet_dbm: snapshot.tx_power / 4,
            bridge_dbm: snapshot.bridge_tx_power / 4,
            pool: snapshot.pool.into(),
            remember_ble: snapshot.remember_ble,
            // The engine does not hold this one, so it comes from the handle.
            remember_bridge: settings.bridge_memory.is_enabled(),
            // Nor this one. It is what the file holds, hand edits included.
            wdgwars_key: settings.saved.api_keys.wdgwars.clone(),
            excluded: snapshot.excluded_ssids.clone(),
            page: Page::Main,
        }
    }

    /// A key while the modal is open.
    pub(super) fn on_key(&mut self, key: KeyEvent) -> ModalAction {
        if matches!(self.page, Page::Excluded { .. }) {
            return self.on_page_key(key);
        }
        if self.selected == Field::ExcludedSsids
            && matches!(key.code, KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'))
        {
            self.page = Page::Excluded {
                cursor: 0,
                input: None,
                error: None,
                before: self.excluded.clone(),
            };
            return ModalAction::Stay;
        }
        // The key row is a text field. Letters type there rather than move or close.
        if self.selected == Field::WdgwarsKey {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('u') if control => {
                    self.wdgwars_key.clear();
                    return ModalAction::Stay;
                }
                KeyCode::Char(c) if !control => {
                    self.wdgwars_key.push(c);
                    return ModalAction::Stay;
                }
                KeyCode::Backspace => {
                    self.wdgwars_key.pop();
                    return ModalAction::Stay;
                }
                KeyCode::Char(_) => return ModalAction::Stay,
                _ => {}
            }
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.selected = self.selected.next(),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.prev(),
            KeyCode::Left | KeyCode::Char('h') => self.step(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step(1),
            KeyCode::Esc | KeyCode::Char('q') => return ModalAction::Close,
            KeyCode::Enter => return ModalAction::Apply,
            _ => {}
        }
        ModalAction::Stay
    }

    /// A key on the excluded-SSIDs page.
    fn on_page_key(&mut self, key: KeyEvent) -> ModalAction {
        let Page::Excluded { cursor, input, error, before } = &mut self.page else {
            return ModalAction::Stay;
        };
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(text) = input {
            match key.code {
                KeyCode::Char('u') if control => {
                    text.clear();
                    *error = None;
                }
                KeyCode::Char(c) if !control => {
                    text.push(c);
                    *error = None;
                }
                KeyCode::Backspace => {
                    text.pop();
                    *error = None;
                }
                KeyCode::Enter => match config::check_excluded_ssid(text, &self.excluded) {
                    Err(why) => *error = Some(refusal_text(why)),
                    Ok(()) => {
                        self.excluded.push(std::mem::take(text));
                        *cursor = self.excluded.len() - 1;
                        *input = None;
                        *error = None;
                    }
                },
                KeyCode::Esc => {
                    *input = None;
                    *error = None;
                }
                _ => {}
            }
            return ModalAction::Stay;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                *cursor = (*cursor + 1).min(self.excluded.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
            KeyCode::Char('a') => *input = Some(String::new()),
            KeyCode::Char('d') | KeyCode::Delete if *cursor < self.excluded.len() => {
                self.excluded.remove(*cursor);
                *cursor = (*cursor).min(self.excluded.len().saturating_sub(1));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.excluded = std::mem::take(before);
                self.page = Page::Main;
            }
            KeyCode::Enter => return ModalAction::SaveExcluded,
            _ => {}
        }
        ModalAction::Stay
    }

    /// Return from the excluded-SSIDs page to the rows, keeping the list as edited.
    pub(super) fn leave_page(&mut self) {
        self.page = Page::Main;
    }

    /// A bracketed paste. Appended to the key when the modal is on its row, or to the
    /// SSID being typed on the excluded-SSIDs page, and ignored anywhere else. Control
    /// characters are dropped from both, so a trailing newline never reaches the file.
    /// The key drops whitespace too, which an SSID may hold.
    pub(super) fn paste(&mut self, text: &str) {
        if let Page::Excluded { input: Some(input), error, .. } = &mut self.page {
            input.extend(text.chars().filter(|c| !c.is_control()));
            *error = None;
            return;
        }
        if self.page != Page::Main || self.selected != Field::WdgwarsKey {
            return;
        }
        self.wdgwars_key.extend(text.chars().filter(|c| !c.is_whitespace() && !c.is_control()));
    }
}

/// A refusal as the page's one-line error shows it. Short, to fit the modal's width;
/// the file's messages quote the entry, which a 32-byte SSID would overflow.
fn refusal_text(refusal: config::SsidRefusal) -> &'static str {
    match refusal {
        config::SsidRefusal::Empty => "type an ssid first",
        config::SsidRefusal::TooLong => "over 32 bytes, the most an ssid holds",
        config::SsidRefusal::Listed => "already listed",
    }
}

/// Send the modal's values to the engine and save them to `wartui.toml`. Returns the
/// notice saying what happened.
///
/// Fails with [`ENGINE_BUSY`] when the engine takes no command, and then saves
/// nothing: the file would claim settings the engine is not running.
pub(super) fn apply(
    modal: &ConfigModal,
    snapshot: &Snapshot,
    commands: &mpsc::Sender<Command>,
    settings: &mut Settings,
) -> Result<String, &'static str> {
    // Reserve every slot first, so a full queue applies none of the modal rather than
    // part of it. The excluded list is not among them: its page sends it on its own
    // `Enter` and restores it on `Esc`, so on the rows it is always what is in force.
    let Ok(mut permits) = commands.try_reserve_many(3) else {
        return Err(ENGINE_BUSY);
    };
    let pool = ChannelPool::from(modal.pool);
    for command in [
        Command::SetPool { pool },
        Command::SetTxPower { nodes: modal.fleet_dbm * 4, bridge: modal.bridge_dbm * 4 },
        Command::RememberBle { on: modal.remember_ble },
    ] {
        if let Some(permit) = permits.next() {
            permit.send(command);
        }
    }
    // Same rule as `toggle_ble`. Power and pool reach a node in an assignment, and a
    // fleet with no plan has none to send.
    let planned = snapshot.plan.is_some();
    let when = if planned { "on their next heartbeat" } else { "once the fleet is in a plan" };
    let mut text = format!(
        "tx power: fleet {} dBm, bridge {} dBm — nodes take it {when}",
        modal.fleet_dbm, modal.bridge_dbm
    );
    if pool != snapshot.pool {
        if planned {
            text.push_str(&format!(
                "; pool {pool} — the fleet re-cuts on each node's next heartbeat"
            ));
        } else {
            text.push_str(&format!("; pool {pool} — nodes take it once the fleet is in a plan"));
        }
    }
    let memory = &settings.bridge_memory;
    if modal.remember_bridge != memory.is_enabled() {
        memory.set_enabled(modal.remember_bridge);
        if modal.remember_bridge {
            // Remember the bridge connected now. A reconnect may never come.
            if let Some(bridge) = &snapshot.bridge {
                memory.remember(bridge.mac);
            }
            text.push_str("; bridge remembered");
        } else {
            text.push_str("; bridge forgotten — each start scans for it");
        }
    }
    text.push_str(&save_outcome(modal, snapshot, settings));
    Ok(text)
}

/// Send the modal's excluded SSIDs to the engine and save them to `wartui.toml`.
/// Returns the notice saying what happened. Nothing outside `excluded-ssids` changes in
/// the file, and no other row goes to the engine.
///
/// Fails with [`ENGINE_BUSY`] when the engine takes no command, and then saves
/// nothing, as [`apply`] does.
pub(super) fn apply_excluded(
    modal: &ConfigModal,
    commands: &mpsc::Sender<Command>,
    settings: &mut Settings,
) -> Result<String, &'static str> {
    let ssids = modal.excluded.iter().map(|ssid| ssid.as_bytes().to_vec()).collect();
    if commands.try_send(Command::SetExcludedSsids { ssids }).is_err() {
        return Err(ENGINE_BUSY);
    }
    let mut written = settings.saved.clone();
    written.excluded_ssids = modal.excluded.clone();
    let saved = save(settings, written)
        .map(|path| format!("; saved to {}", path.display()))
        .unwrap_or_else(|error| error);
    Ok(format!("excluded ssids: {}{saved}", excluded_count(&modal.excluded)))
}

/// Write every modal row to `wartui.toml`, and return the notice suffix: where it
/// saved, or why it did not.
///
/// Overwrites the whole file, hand edits included. The rows are already applied, so a
/// failed save loses only the file.
///
/// With remember on, the node saved is the remembered one, else the current holder of
/// the scan. The engine makes the same choice when the row turns on, and it keeps a
/// remembered node that has not been heard from yet. On success the written config
/// becomes [`Settings::saved`].
fn save_outcome(modal: &ConfigModal, snapshot: &Snapshot, settings: &mut Settings) -> String {
    let written = config::Config {
        pool: Some(modal.pool),
        excluded_ssids: modal.excluded.clone(),
        tx_power: config::TxPower { fleet: Some(modal.fleet_dbm), bridge: Some(modal.bridge_dbm) },
        bluetooth: config::Bluetooth {
            remember: Some(modal.remember_ble),
            node: if modal.remember_ble {
                snapshot.preferred_ble.or(snapshot.ble_node)
            } else {
                None
            },
        },
        bridge: config::Bridge { remember: Some(modal.remember_bridge) },
        api_keys: config::ApiKeys { wdgwars: modal.wdgwars_key.trim().to_owned() },
    };
    save(settings, written)
        .map(|path| format!("; saved to {}", path.display()))
        .unwrap_or_else(|error| error)
}

/// Save the Bluetooth node `b` just chose to `wartui.toml`, and return the notice
/// suffix in the style of [`save_outcome`]. Nothing outside `[bluetooth]` changes.
pub(super) fn save_ble_node(settings: &mut Settings, node: Option<Mac>) -> String {
    // Force `remember = true`. `b` saves only while the engine remembers, but after a
    // failed save `saved` may still say off, and `load` rejects off beside a node.
    let mut written = settings.saved.clone();
    written.bluetooth = config::Bluetooth { remember: Some(true), node };
    let done = if node.is_some() { "remembered" } else { "forgotten" };
    save(settings, written)
        .map(|path| format!("; {done} in {}", path.display()))
        .unwrap_or_else(|error| error)
}

/// Write `written` to the config path, and on success make it [`Settings::saved`].
/// Returns the path, or the notice suffix saying why nothing was written.
fn save(settings: &mut Settings, written: config::Config) -> Result<PathBuf, String> {
    let Some(path) = settings.config_path.clone() else {
        return Err("; nowhere to save it — use --config".to_owned());
    };
    config::save(&path, &written).map_err(|error| format!("; could not save: {error}"))?;
    settings.saved = written;
    Ok(path)
}

/// The settings modal, centered over the live view behind it.
pub(super) fn draw_settings_modal(frame: &mut Frame<'_>, modal: &ConfigModal) {
    let area = centered_rect(48, 14, frame.area());
    frame.render_widget(Clear, area);

    if let Page::Excluded { cursor, input, error, .. } = &modal.page {
        let block = Block::bordered().title(" settings › excluded ssids ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = excluded_page_lines(
            &modal.excluded,
            *cursor,
            input.as_deref(),
            *error,
            usize::from(inner.height),
        );
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let block = Block::bordered().title(" settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let styled = |field: Field| {
        if modal.selected == field {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        }
    };
    let row = |label: &str, value: &dyn Display, field: Field| {
        Line::from(Span::styled(format!("{label:<18}◂ {value} ▸"), styled(field)))
    };
    let dbm = |dbm: i8| format!("{dbm:>2} dBm");
    let on_off = |on: bool| if on { "on" } else { "off" };
    let lines = vec![
        row("pool", &ChannelPool::from(modal.pool), Field::Pool),
        row("fleet tx power", &dbm(modal.fleet_dbm), Field::Fleet),
        row("bridge tx power", &dbm(modal.bridge_dbm), Field::Bridge),
        row("remember bt node", &on_off(modal.remember_ble), Field::RememberBle),
        row("remember bridge", &on_off(modal.remember_bridge), Field::RememberBridge),
        Line::from(Span::styled(
            format!("{:<18}  {} ›", "excluded ssids", excluded_count(&modal.excluded)),
            styled(Field::ExcludedSsids),
        )),
        Line::default(),
        Line::from("api keys"),
        Line::from(Span::styled(
            format!("{:<18}  {}", "wdgwars", mask_key(&modal.wdgwars_key)),
            styled(Field::WdgwarsKey),
        )),
        Line::default(),
        // The one key the row needs that nothing else on screen says.
        Line::from(match modal.selected {
            Field::WdgwarsKey if !modal.wdgwars_key.is_empty() => {
                "ctrl-u clear · enter save · esc cancel"
            }
            Field::ExcludedSsids => "enter open · esc cancel",
            _ => "enter save · esc cancel",
        }),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The excluded-SSIDs row's value: how many, or `none`.
fn excluded_count(excluded: &[String]) -> String {
    if excluded.is_empty() { "none".to_owned() } else { excluded.len().to_string() }
}

/// The excluded-SSIDs page in `height` lines: the list, scrolled to keep `cursor` in
/// view, then a line for a refusal, a line for the SSID being typed, and the keys.
fn excluded_page_lines(
    excluded: &[String],
    cursor: usize,
    input: Option<&str>,
    error: Option<&'static str>,
    height: usize,
) -> Vec<Line<'static>> {
    let rows = height.saturating_sub(3).max(1);
    let mut lines: Vec<Line<'static>> = if excluded.is_empty() {
        vec![Line::from("(none)")]
    } else {
        let top = cursor.saturating_sub(rows - 1);
        excluded
            .iter()
            .enumerate()
            .skip(top)
            .take(rows)
            .map(|(index, ssid)| {
                let style = if index == cursor && input.is_none() {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Line::from(Span::styled(ssid.clone(), style))
            })
            .collect()
    };
    lines.resize(rows, Line::default());
    lines.push(error.map(Line::from).unwrap_or_default());
    lines.push(input.map(|text| Line::from(format!("add: {text}▏"))).unwrap_or_default());
    lines.push(Line::from(if input.is_some() {
        "enter add · ctrl-u clear · esc cancel"
    } else {
        "a add · d remove · enter save · esc cancel"
    }));
    lines
}

/// A key as the modal shows it, so a screen share or a photo does not leak it.
///
/// | Length | Shown |
/// |---|---|
/// | 0 | `(not set, type or paste)` |
/// | 1–3 | the whole key |
/// | 4–9 | `•` × (length − 3), then the last 3 |
/// | 10+ | the first 1–3, `•` × 6, then the last 3 |
///
/// The last three show so the operator sees what they typed. On a long key the first
/// few show too, so it reads as which key it is, while its middle and length stay
/// hidden.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let n = chars.len();
    if n == 0 {
        return "(not set, type or paste)".to_owned();
    }
    let tail = n.min(3);
    let head = n.saturating_sub(9).min(3);
    let dots = (n - head - tail).min(6);
    let mut shown: String = chars[..head].iter().collect();
    shown.push_str(&"•".repeat(dots));
    shown.extend(&chars[n - tail..]);
    shown
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use wartui_proto::plan::plan;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    #[test]
    fn ui_opens_modal_with_snapshot_values_when_c_key_is_pressed() {
        let mut snapshot = busy();
        snapshot.tx_power = 40; // 10 dBm
        snapshot.bridge_tx_power = 60; // 15 dBm
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let modal = ui.modal().expect("the modal opened");
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

        let modal = ui.modal().expect("the modal opened");
        assert_eq!(modal.pool, PoolArg::Eu);
    }

    #[test]
    fn ui_reaches_bridge_row_from_pool_when_j_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").selected, Field::Fleet);

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(ui.modal().expect("still open").selected, Field::Bridge);
    }

    #[test]
    fn ui_steps_pool_and_wraps_at_ends_when_h_or_l_pressed() {
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::All;
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").selected, Field::Pool, "opens on the pool row");

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(
            ui.modal().expect("still open").pool,
            PoolArg::Us,
            "wraps from the start to the end"
        );

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(
            ui.modal().expect("still open").pool,
            PoolArg::All,
            "wraps from the end to the start"
        );

        for expected in [PoolArg::Eu, PoolArg::Us, PoolArg::All] {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
            assert_eq!(
                ui.modal().expect("still open").pool,
                expected,
                "steps forward one at a time"
            );
        }
    }

    #[test]
    fn ui_closes_modal_without_sending_a_command_when_esc_or_q_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        for closer in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut ui = Ui::default();
            ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
            assert!(ui.modal().is_some(), "the modal is open");

            ui.on_modal_key(KeyEvent::new(closer, KeyModifiers::NONE), &snapshot, &tx);

            assert!(ui.modal().is_none(), "{closer:?} closes it");
            assert!(rx.try_recv().is_err(), "{closer:?} cancels rather than applies");
        }
    }

    #[test]
    fn ui_modal_wraps_dbm_between_two_and_twenty_when_stepping_past_the_range() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);

        while ui.modal().expect("still open").fleet_dbm > 2 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        }

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").fleet_dbm, 20, "wraps below the floor");

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").fleet_dbm, 2, "wraps past the ceiling");
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

        assert_eq!(rx.try_recv().unwrap(), Command::SetPool { pool: snapshot.pool });
        assert_eq!(rx.try_recv().unwrap(), Command::SetTxPower { nodes: 44, bridge: 60 });
        assert!(ui.modal().is_none(), "applying closes the modal");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("fleet 11 dBm, bridge 15 dBm"), "{notice}");
    }

    #[test]
    fn ui_says_nodes_take_it_once_fleet_is_in_a_plan_when_no_plan_exists() {
        // Same rule as `toggle_ble`. The power arrives in an assignment, so it waits
        // for a plan.
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
    fn ui_sends_pool_command_when_enter_pressed_with_pool_changed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::All;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetPool { pool: ChannelPool::Eu });
        assert_eq!(rx.try_recv().unwrap(), Command::SetTxPower { nodes: 8, bridge: 8 });
        assert_eq!(rx.try_recv().unwrap(), Command::RememberBle { on: true });
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        // `busy()`'s plan is `None`, so the re-cut waits on one.
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("pool EU — nodes take it once the fleet is in a plan"), "{notice}");
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
        // Holds the scan but is not remembered, so the save falls back on the holder.
        snapshot.ble_node = Some(snapshot.nodes[0].state.mac);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_ok(), "the live change still goes out");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains(&target.display().to_string()), "{notice}");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.pool, Some(PoolArg::Eu));
        assert_eq!(saved.tx_power.fleet, Some(10));
        assert_eq!(saved.tx_power.bridge, Some(15));
        assert_eq!(saved.bluetooth.remember, Some(true));
        assert_eq!(saved.bluetooth.node, Some(snapshot.nodes[0].state.mac));
    }

    #[test]
    fn ui_sends_nothing_and_saves_nothing_when_enter_pressed_with_one_queue_slot_free() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(2);
        tx.try_send(Command::ClearRing { mac: None }).unwrap();
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        // Room for one of the modal's three commands, so none goes out.
        assert_eq!(rx.try_recv().unwrap(), Command::ClearRing { mac: None });
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        assert!(!target.exists(), "nothing was written");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("not accepting commands"), "{notice}");
    }

    #[test]
    fn ui_keeps_modal_open_with_edits_when_enter_pressed_with_engine_busy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(2);
        tx.try_send(Command::ClearRing { mac: None }).unwrap();
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        // Open with the edit, so the operator can press Enter again once the engine
        // takes commands.
        let modal = ui.modal().expect("still open");
        assert_eq!(modal.fleet_dbm, 3, "the edit survives");
        assert!(!target.exists(), "nothing was written");
        assert_eq!(ui.notice(snapshot.now_ms), Some(ENGINE_BUSY));
    }

    #[test]
    fn ui_sends_nothing_and_saves_nothing_when_enter_pressed_with_queue_smaller_than_three() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(2);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "nothing was sent");
        assert!(!target.exists(), "nothing was written");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("not accepting commands"), "{notice}");
    }

    #[test]
    fn ui_sends_remember_ble_and_saves_it_when_enter_pressed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = busy();
        snapshot.ble_node = Some(snapshot.nodes[0].state.mac);
        snapshot.preferred_ble = snapshot.ble_node;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        for _ in 0..3 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        }
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(matches!(rx.try_recv().unwrap(), Command::SetPool { .. }));
        assert!(matches!(rx.try_recv().unwrap(), Command::SetTxPower { .. }));
        assert_eq!(rx.try_recv().unwrap(), Command::RememberBle { on: false });
        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.bluetooth.remember, Some(false));
        assert_eq!(file.bluetooth.node, None, "turning it off forgets the node");
        assert_eq!(ui.saved().bluetooth.remember, Some(false), "the view knows it too");
    }

    #[test]
    fn ui_toggles_remember_row_when_h_or_l_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        for _ in 0..3 {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        }
        let modal = ui.modal().expect("still open");
        assert_eq!(modal.selected, Field::RememberBle, "the row under the powers");
        assert!(modal.remember_ble, "seeded from the snapshot");

        for (key, expected) in [('h', false), ('h', true), ('l', false), ('l', true)] {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE), &snapshot, &tx);
            assert_eq!(ui.modal().expect("still open").remember_ble, expected, "{key} flips it");
        }
    }

    #[test]
    fn ui_overwrites_hand_edit_made_mid_run_when_enter_pressed() {
        // Enter saves what the modal shows, not what the file holds. A hand edit made
        // while the modal is open is overwritten like anything else.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "pool = \"us\"\n[tx-power]\nfleet = 5\nbridge = 6\n").unwrap();
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Eu;
        snapshot.tx_power = 40; // 10 dBm
        snapshot.bridge_tx_power = 60; // 15 dBm
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
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
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);
        let after_first = std::fs::read_to_string(&target).expect("written by the first save");

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let after_second = std::fs::read_to_string(&target).expect("still there");
        assert_eq!(after_first, after_second, "the second save touched nothing");
    }

    #[test]
    fn ui_omits_pool_from_notice_when_enter_pressed_with_pool_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        // Only the fleet row moves. The pool row is left alone.
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("; pool"), "{notice}");
    }

    #[test]
    fn ui_reports_nowhere_to_save_when_no_config_path_is_set() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings::default());
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("use --config"), "{notice}");
        assert!(notice.contains("pool EU"), "the moved pool row is applied anyway: {notice}");
    }

    #[test]
    fn ui_applies_pool_when_save_fails_with_pool_moved() {
        // A regular file where the config's directory should be, so the save
        // cannot create it.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings {
            config_path: Some(blocker.join("wartui.toml")),
            ..Settings::default()
        });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("could not save"), "{notice}");
        // The engine has the pool whether or not the file does.
        assert_eq!(rx.try_recv().unwrap(), Command::SetPool { pool: ChannelPool::Eu });
    }

    #[test]
    fn ui_says_fleet_recuts_on_next_heartbeat_when_pool_moved_with_plan() {
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::All;
        snapshot.plan = plan(ChannelPool::All, 4);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(
            notice.contains("pool EU — the fleet re-cuts on each node's next heartbeat"),
            "{notice}"
        );
    }

    /// A view saving to `dir/wartui.toml`, remembering the bridge in `dir/bridge`.
    fn ui_with_bridge_memory(dir: &std::path::Path, memory: &BridgeMemory) -> Ui {
        Ui::new(Settings {
            config_path: Some(dir.join("wartui.toml")),
            bridge_memory: memory.clone(),
            ..Settings::default()
        })
    }

    /// Open the modal and walk down to `field`. `↓` rather than `j`, which the key row
    /// would type.
    fn select(field: Field, ui: &mut Ui, snapshot: &Snapshot, tx: &mpsc::Sender<Command>) {
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), snapshot, tx);
        while ui.modal().expect("open").selected != field {
            ui.on_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), snapshot, tx);
        }
    }

    /// Open the modal and walk down to the `remember bridge` row.
    fn select_remember_bridge(ui: &mut Ui, snapshot: &Snapshot, tx: &mpsc::Sender<Command>) {
        select(Field::RememberBridge, ui, snapshot, tx);
    }

    #[test]
    fn ui_toggles_remember_bridge_row_when_h_or_l_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select_remember_bridge(&mut ui, &snapshot, &tx);
        let modal = ui.modal().expect("still open");
        assert_eq!(modal.selected, Field::RememberBridge, "the row under remember bt node");
        assert!(modal.remember_bridge, "seeded from the handle, on by default");

        for (key, expected) in [('h', false), ('h', true), ('l', false), ('l', true)] {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE), &snapshot, &tx);
            assert_eq!(ui.modal().expect("still open").remember_bridge, expected, "{key} flips it");
        }
    }

    #[test]
    fn ui_forgets_bridge_and_saves_off_when_remember_bridge_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bridge");
        let memory = BridgeMemory::at(&file);
        memory.remember([0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24]);
        assert!(file.exists());
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        select_remember_bridge(&mut ui, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(!file.exists(), "off means no file");
        assert!(!memory.is_enabled(), "the transport's clone sees it too");
        let saved = config::load(Some(&dir.path().join("wartui.toml"))).expect("a valid file");
        assert_eq!(saved.bridge.remember, Some(false));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("bridge forgotten"), "{notice}");
    }

    #[test]
    fn ui_remembers_connected_bridge_when_remember_bridge_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bridge");
        let memory = BridgeMemory::at(&file);
        memory.set_enabled(false);
        let snapshot = busy();
        let bridge = snapshot.bridge.as_ref().expect("a bridge").mac;
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        select_remember_bridge(&mut ui, &snapshot, &tx);
        assert!(!ui.modal().expect("still open").remember_bridge, "seeded off");
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(memory.is_enabled());
        assert_eq!(BridgeMemory::at(&file).recall(), Some(bridge), "the bridge connected now");
        let saved = config::load(Some(&dir.path().join("wartui.toml"))).expect("a valid file");
        assert_eq!(saved.bridge.remember, Some(true));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("bridge remembered"), "{notice}");
    }

    #[test]
    fn ui_leaves_bridge_file_alone_when_remember_bridge_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bridge");
        let memory = BridgeMemory::at(&file);
        // A board other than the one connected. An unchanged row must not rewrite it.
        let other = [0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0];
        memory.remember(other);
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(memory.is_enabled());
        assert_eq!(memory.recall(), Some(other));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("bridge forgotten"), "{notice}");
        assert!(!notice.contains("bridge remembered"), "{notice}");
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
        assert!(rendered.contains("remember bridge"), "{rendered}");
        // The view behind it is still live, not blanked out.
        assert!(rendered.contains("C5 57:84"), "{rendered}");
    }

    /// Open the modal and walk down to the `wdgwars` row.
    fn select_wdgwars(ui: &mut Ui, snapshot: &Snapshot, tx: &mpsc::Sender<Command>) {
        select(Field::WdgwarsKey, ui, snapshot, tx);
    }

    fn press(ui: &mut Ui, code: KeyCode, modifiers: KeyModifiers, snapshot: &Snapshot) {
        let (tx, _rx) = mpsc::channel(4);
        ui.on_modal_key(KeyEvent::new(code, modifiers), snapshot, &tx);
    }

    #[test]
    fn ui_types_letters_into_key_when_wdgwars_row_selected() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select_wdgwars(&mut ui, &snapshot, &tx);
        assert_eq!(FIELDS.last(), Some(&Field::WdgwarsKey), "the last row");

        for c in "hjkq".chars() {
            press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE, &snapshot);
        }

        let modal = ui.modal().expect("q types rather than closes");
        assert_eq!(modal.selected, Field::WdgwarsKey, "j and k type rather than move");
        assert_eq!(modal.wdgwars_key, "hjkq");
        assert!(rx.try_recv().is_err(), "nothing was sent");
    }

    #[test]
    fn ui_edits_key_when_backspace_or_ctrl_u_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select_wdgwars(&mut ui, &snapshot, &tx);
        for c in "abc".chars() {
            press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE, &snapshot);
        }

        press(&mut ui, KeyCode::Backspace, KeyModifiers::NONE, &snapshot);
        assert_eq!(ui.modal().expect("open").wdgwars_key, "ab");

        press(&mut ui, KeyCode::Char('u'), KeyModifiers::CONTROL, &snapshot);
        assert_eq!(ui.modal().expect("open").wdgwars_key, "");
    }

    #[test]
    fn ui_leaves_key_row_when_up_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select_wdgwars(&mut ui, &snapshot, &tx);

        press(&mut ui, KeyCode::Up, KeyModifiers::NONE, &snapshot);

        assert_eq!(ui.modal().expect("open").selected, Field::ExcludedSsids);
    }

    #[test]
    fn ui_appends_paste_without_whitespace_when_wdgwars_row_selected() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select_wdgwars(&mut ui, &snapshot, &tx);
        press(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE, &snapshot);

        ui.on_modal_paste(" abc\tdef\r\n");

        let modal = ui.modal().expect("a pasted newline does not save");
        assert_eq!(modal.wdgwars_key, "xabcdef");
    }

    #[test]
    fn ui_ignores_paste_when_another_row_is_selected_or_modal_closed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_modal_paste("abc");
        assert!(ui.modal().is_none(), "a paste opens nothing");

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_paste("abc");

        let modal = ui.modal().expect("open");
        assert_eq!(modal.selected, Field::Pool);
        assert_eq!(modal.wdgwars_key, "");
    }

    #[test]
    fn ui_saves_key_that_reloads_when_enter_pressed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        select_wdgwars(&mut ui, &snapshot, &tx);
        ui.on_modal_paste("abc123def456");

        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE, &snapshot);

        assert!(ui.modal().is_none(), "Enter saves and closes on the key row too");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.api_keys.wdgwars, "abc123def456");
        assert_eq!(ui.saved().api_keys.wdgwars, "abc123def456", "the view knows it too");
    }

    #[test]
    fn ui_keeps_hand_written_key_when_enter_pressed_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[api-keys]\nwdgwars = \"by-hand-key\"\n").unwrap();
        let saved = config::load(Some(&target)).unwrap();
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui::new(Settings { config_path: Some(target.clone()), saved, ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").wdgwars_key, "by-hand-key", "seeded");

        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE, &snapshot);

        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.api_keys.wdgwars, "by-hand-key");
    }

    #[test]
    fn mask_key_says_not_set_when_key_is_empty() {
        assert_eq!(mask_key(""), "(not set, type or paste)");
    }

    #[test]
    fn mask_key_shows_tail_and_grows_head_when_key_lengthens() {
        for (key, shown) in [
            ("a", "a"),
            ("abc", "abc"),
            ("abcde", "••cde"),
            ("abcdefghi", "••••••ghi"),
            ("abcdefghij", "a••••••hij"),
            ("abcdefghijk", "ab••••••ijk"),
            ("abcdefghijkl", "abc••••••jkl"),
            ("abcdefghijklmnopqrst", "abc••••••rst"),
        ] {
            assert_eq!(mask_key(key), shown, "{} characters", key.len());
        }
    }

    #[test]
    fn draw_shows_masked_key_and_never_the_whole_key_when_modal_open() {
        let snapshot = busy();
        let saved = config::Config {
            api_keys: config::ApiKeys { wdgwars: "abcSECRETxyz".to_owned() },
            ..config::Config::default()
        };
        let mut ui = Ui::new(Settings { saved, ..Settings::default() });
        let (tx, _rx) = mpsc::channel(4);
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("api keys"), "{rendered}");
        assert!(rendered.contains("wdgwars"), "{rendered}");
        assert!(rendered.contains("abc••••••xyz"), "{rendered}");
        assert!(!rendered.contains("SECRET"), "{rendered}");
        assert!(rendered.contains("enter save"), "the modal is tall enough: {rendered}");
    }

    /// The modal drawn at 120×30, opened with `key` saved and the cursor on `field`.
    fn modal_rendered(key: &str, field: Field) -> String {
        let snapshot = busy();
        let saved = config::Config {
            api_keys: config::ApiKeys { wdgwars: key.to_owned() },
            ..config::Config::default()
        };
        let mut ui = Ui::new(Settings { saved, ..Settings::default() });
        let (tx, _rx) = mpsc::channel(4);
        select(field, &mut ui, &snapshot, &tx);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        terminal.backend().to_string()
    }

    #[test]
    fn draw_shows_ctrl_u_tip_when_key_row_selected_with_key_entered() {
        let rendered = modal_rendered("abc123def456", Field::WdgwarsKey);
        assert!(rendered.contains("ctrl-u clear · enter save · esc cancel"), "{rendered}");
    }

    #[test]
    fn draw_omits_ctrl_u_tip_when_key_row_selected_with_key_empty() {
        let rendered = modal_rendered("", Field::WdgwarsKey);
        assert!(rendered.contains("(not set, type or paste)"), "fits unclipped: {rendered}");
        assert!(!rendered.contains("ctrl-u"), "{rendered}");
        assert!(rendered.contains("enter save · esc cancel"), "{rendered}");
    }

    #[test]
    fn draw_omits_ctrl_u_tip_when_another_row_selected_with_key_entered() {
        let rendered = modal_rendered("abc123def456", Field::RememberBridge);
        assert!(!rendered.contains("ctrl-u"), "{rendered}");
        assert!(rendered.contains("enter save · esc cancel"), "{rendered}");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Type `text` and press `Enter` in the page's input, starting it with `a`.
    fn add_ssid(ui: &mut Ui, text: &str, snapshot: &Snapshot) {
        press(ui, KeyCode::Char('a'), KeyModifiers::NONE, snapshot);
        for c in text.chars() {
            press(ui, KeyCode::Char(c), KeyModifiers::NONE, snapshot);
        }
        press(ui, KeyCode::Enter, KeyModifiers::NONE, snapshot);
    }

    /// The modal opened on `snapshot` with its excluded-SSIDs page open.
    fn open_excluded_page(snapshot: &Snapshot, settings: Settings) -> Ui {
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(settings);
        select(Field::ExcludedSsids, &mut ui, snapshot, &tx);
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE, snapshot);
        ui
    }

    fn excluded_snapshot(ssids: &[&str]) -> Snapshot {
        let mut snapshot = busy();
        snapshot.excluded_ssids = ssids.iter().map(|&ssid| ssid.to_owned()).collect();
        snapshot
    }

    #[test]
    fn ui_opens_excluded_page_when_enter_right_or_l_pressed_on_its_row() {
        let snapshot = excluded_snapshot(&["HomeNet"]);
        for opener in [KeyCode::Enter, KeyCode::Right, KeyCode::Char('l')] {
            let (tx, mut rx) = mpsc::channel(4);
            let mut ui = Ui::default();
            select(Field::ExcludedSsids, &mut ui, &snapshot, &tx);

            ui.on_modal_key(key(opener), &snapshot, &tx);

            let modal = ui.modal().expect("still open");
            assert!(matches!(modal.page, Page::Excluded { .. }), "{opener:?} opens the page");
            assert_eq!(modal.excluded, ["HomeNet"], "seeded from the snapshot");
            assert!(rx.try_recv().is_err(), "{opener:?} sends nothing");
        }
    }

    #[test]
    fn ui_adds_ssid_with_spaces_when_typed_and_enter_pressed_on_excluded_page() {
        let snapshot = busy();
        let mut ui = open_excluded_page(&snapshot, Settings::default());

        add_ssid(&mut ui, "Pixel 9", &snapshot);

        let modal = ui.modal().expect("still open");
        assert_eq!(modal.excluded, ["Pixel 9"]);
        assert!(matches!(modal.page, Page::Excluded { input: None, error: None, .. }));
    }

    #[test]
    fn ui_refuses_ssid_with_error_when_duplicate_empty_or_over_32_bytes() {
        let snapshot = excluded_snapshot(&["HomeNet"]);
        for (typed, why) in [
            ("HomeNet", "already listed"),
            ("", "type an ssid first"),
            (&"x".repeat(33), "over 32 bytes, the most an ssid holds"),
        ] {
            let mut ui = open_excluded_page(&snapshot, Settings::default());

            add_ssid(&mut ui, typed, &snapshot);

            let modal = ui.modal().expect("still open");
            assert_eq!(modal.excluded, ["HomeNet"], "{typed:?} is not added");
            assert!(
                matches!(&modal.page, Page::Excluded { input: Some(_), error: Some(e), .. } if *e == why),
                "{typed:?}: {:?}",
                modal.page
            );
        }
    }

    #[test]
    fn ui_cancels_only_the_input_when_esc_pressed_while_typing_on_excluded_page() {
        let snapshot = excluded_snapshot(&["HomeNet"]);
        let mut ui = open_excluded_page(&snapshot, Settings::default());
        add_ssid(&mut ui, "Pixel 9", &snapshot);
        press(&mut ui, KeyCode::Char('a'), KeyModifiers::NONE, &snapshot);
        press(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE, &snapshot);

        press(&mut ui, KeyCode::Esc, KeyModifiers::NONE, &snapshot);

        let modal = ui.modal().expect("still open");
        assert!(matches!(modal.page, Page::Excluded { input: None, .. }), "still on the page");
        assert_eq!(modal.excluded, ["HomeNet", "Pixel 9"], "the earlier add survives");
    }

    #[test]
    fn ui_removes_selected_ssid_when_d_or_delete_pressed_on_excluded_page() {
        let snapshot = excluded_snapshot(&["HomeNet", "Pixel 9", "Work"]);
        let mut ui = open_excluded_page(&snapshot, Settings::default());
        press(&mut ui, KeyCode::Down, KeyModifiers::NONE, &snapshot);

        press(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE, &snapshot);
        assert_eq!(ui.modal().expect("open").excluded, ["HomeNet", "Work"]);

        press(&mut ui, KeyCode::Delete, KeyModifiers::NONE, &snapshot);
        assert_eq!(ui.modal().expect("open").excluded, ["HomeNet"], "the cursor stays in range");
    }

    #[test]
    fn ui_discards_page_edits_and_returns_to_rows_when_esc_or_q_pressed_on_excluded_page() {
        let snapshot = excluded_snapshot(&["HomeNet"]);
        for closer in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut ui = open_excluded_page(&snapshot, Settings::default());
            add_ssid(&mut ui, "Pixel 9", &snapshot);
            press(&mut ui, KeyCode::Up, KeyModifiers::NONE, &snapshot);
            press(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE, &snapshot);

            press(&mut ui, closer, KeyModifiers::NONE, &snapshot);

            let modal = ui.modal().expect("the modal stays open");
            assert_eq!(modal.page, Page::Main, "{closer:?} backs out");
            assert_eq!(modal.selected, Field::ExcludedSsids);
            assert_eq!(modal.excluded, ["HomeNet"], "{closer:?} drops the page's edits");
        }
    }

    #[test]
    fn ui_appends_paste_without_control_characters_when_typing_on_excluded_page() {
        let snapshot = busy();
        let mut ui = open_excluded_page(&snapshot, Settings::default());
        press(&mut ui, KeyCode::Char('a'), KeyModifiers::NONE, &snapshot);

        ui.on_modal_paste("My Net\r\n");

        let modal = ui.modal().expect("open");
        assert!(
            matches!(&modal.page, Page::Excluded { input: Some(text), .. } if text == "My Net")
        );
    }

    #[test]
    fn ui_sends_and_saves_only_excluded_ssids_and_returns_to_rows_when_enter_pressed_on_excluded_page()
     {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = excluded_snapshot(&["HomeNet"]);
        snapshot.pool = ChannelPool::All;
        let saved = config::Config { pool: Some(PoolArg::Us), ..config::Config::default() };
        let settings = Settings { config_path: Some(target.clone()), saved, ..Settings::default() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(settings);
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);
        // A pool edit left pending on the rows.
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        while ui.modal().expect("open").selected != Field::ExcludedSsids {
            ui.on_modal_key(key(KeyCode::Down), &snapshot, &tx);
        }
        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);
        add_ssid(&mut ui, "Pixel 9", &snapshot);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(
            rx.try_recv().unwrap(),
            Command::SetExcludedSsids {
                ssids: [b"HomeNet".to_vec(), b"Pixel 9".to_vec()].into_iter().collect()
            }
        );
        assert!(rx.try_recv().is_err(), "the pending pool edit is not sent");
        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.excluded_ssids, ["HomeNet", "Pixel 9"]);
        assert_eq!(file.pool, Some(PoolArg::Us), "the rest of the file is as `saved` held it");
        let modal = ui.modal().expect("the modal stays open");
        assert_eq!(modal.page, Page::Main);
        assert_eq!(modal.selected, Field::ExcludedSsids);
        assert_eq!(modal.pool, PoolArg::Eu, "the pool edit stays pending");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.starts_with("excluded ssids: 2; saved to "), "{notice}");
    }

    #[test]
    fn ui_says_none_when_enter_pressed_on_excluded_page_with_list_emptied() {
        let snapshot = excluded_snapshot(&["HomeNet"]);
        let mut ui = open_excluded_page(&snapshot, Settings::default());
        press(&mut ui, KeyCode::Char('d'), KeyModifiers::NONE, &snapshot);
        let (tx, _rx) = mpsc::channel(4);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert_eq!(notice, "excluded ssids: none; nowhere to save it — use --config");
    }

    #[test]
    fn ui_keeps_excluded_list_in_force_when_esc_pressed_after_enter_on_excluded_page() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let settings = Settings { config_path: Some(target.clone()), ..Settings::default() };
        let mut ui = open_excluded_page(&snapshot, settings);
        add_ssid(&mut ui, "HomeNet", &snapshot);
        let (tx, mut rx) = mpsc::channel(4);
        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Esc), &snapshot, &tx);

        assert!(ui.modal().is_none(), "Esc on the rows closes");
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::SetExcludedSsids { ssids: [b"HomeNet".to_vec()].into_iter().collect() }
        );
        assert!(rx.try_recv().is_err(), "Esc sends nothing to undo it");
        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.excluded_ssids, ["HomeNet"]);
    }

    #[test]
    fn ui_clears_refusal_when_ctrl_u_pressed_while_typing_ssid() {
        let snapshot = excluded_snapshot(&["HomeNet"]);
        let mut ui = open_excluded_page(&snapshot, Settings::default());
        press(&mut ui, KeyCode::Char('a'), KeyModifiers::NONE, &snapshot);
        for c in "HomeNet".chars() {
            press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE, &snapshot);
        }
        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE, &snapshot);
        let Page::Excluded { error, .. } = &ui.modal().expect("open").page else {
            panic!("still on the page");
        };
        assert_eq!(*error, Some("already listed"));

        press(&mut ui, KeyCode::Char('u'), KeyModifiers::CONTROL, &snapshot);

        let Page::Excluded { input, error, .. } = &ui.modal().expect("open").page else {
            panic!("still on the page");
        };
        assert_eq!(input.as_deref(), Some(""));
        assert_eq!(*error, None, "the refusal was about the text just cleared");
    }

    #[test]
    fn ui_keeps_excluded_page_open_with_edits_when_enter_pressed_with_engine_busy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let settings = Settings { config_path: Some(target.clone()), ..Settings::default() };
        let mut ui = open_excluded_page(&snapshot, settings);
        add_ssid(&mut ui, "HomeNet", &snapshot);
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(Command::ClearRing { mac: None }).unwrap();

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::ClearRing { mac: None });
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        assert!(!target.exists(), "nothing was written");
        let modal = ui.modal().expect("still open");
        assert!(matches!(modal.page, Page::Excluded { .. }), "still on the page");
        assert_eq!(modal.excluded, ["HomeNet"]);
        assert_eq!(ui.notice(snapshot.now_ms), Some(ENGINE_BUSY));
    }

    #[test]
    fn draw_shows_excluded_ssids_count_when_modal_open() {
        let snapshot = excluded_snapshot(&["HomeNet", "Pixel 9", "Work"]);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::ExcludedSsids, &mut ui, &snapshot, &tx);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("excluded ssids      3 ›"), "{rendered}");
        assert!(rendered.contains("enter open · esc cancel"), "{rendered}");
    }

    #[test]
    fn draw_shows_none_on_excluded_ssids_row_when_list_empty() {
        let rendered = modal_rendered("", Field::Pool);
        assert!(rendered.contains("excluded ssids      none ›"), "{rendered}");
        assert!(
            rendered.contains("enter save · esc cancel"),
            "the modal is tall enough: {rendered}"
        );
    }

    #[test]
    fn draw_shows_excluded_page_with_list_and_keys_when_page_open() {
        let snapshot = excluded_snapshot(&["HomeNet", "Pixel 9"]);
        let mut ui = open_excluded_page(&snapshot, Settings::default());

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("settings › excluded ssids"), "{rendered}");
        assert!(rendered.contains("HomeNet"), "{rendered}");
        assert!(rendered.contains("Pixel 9"), "{rendered}");
        assert!(rendered.contains("a add · d remove · enter save · esc cancel"), "{rendered}");
    }

    #[test]
    fn excluded_page_keeps_cursor_in_view_when_list_is_longer_than_page() {
        let names: Vec<String> = (0..20).map(|n| format!("net-{n:02}")).collect();
        let lines = excluded_page_lines(&names, 19, None, None, 12);

        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(text.len(), 12, "fills the page exactly");
        assert_eq!(text[8], "net-19", "the cursor's row is the last one listed");
        assert_eq!(text[0], "net-11");
    }
}
