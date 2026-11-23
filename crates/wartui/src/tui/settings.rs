//! The settings modal shows what is in force, not what `wartui.toml` holds. The two
//! differ after a hand edit or a failed save, and the operator is changing the running
//! capture. `Enter` puts every row in force, then writes every row to the file.
//!
//! `Enter` on the excluded-SSIDs page applies and saves only the list, then returns to
//! the rows. The operator opened the page to change the list, not the rows above it, so
//! their pending edits stay pending for the main `Enter` or `Esc`. `Esc` on the page drops
//! only the page's edits, so backing out of a list never costs the rows above it either.
//!
//! The pool, fan, bluetooth, wardrive esp and sd card log rows are the bridge's, and are never
//! written to `wartui.toml`. Each shows what the bridge last reported, and `Enter` sends it only
//! when the row moved. The bridge stores the pool, fan, wardrive esp and log and boots into them.
//! It keeps no scan mode, so the bluetooth row is held by the run, which starts the scan in that
//! mode again after the bridge reboots. The bridge picks which node scans Bluetooth.
//!
//! The sd card's format row is an action, not a setting: nothing in it is applied or saved. Its
//! value is drawn from the live snapshot on every frame, so a format's progress shows while the
//! modal is open. `Enter` on it asks [`super::ui::Ui`] to confirm a format, which erases the card,
//! and the modal stays open behind the confirm with its edits.
//!
//! The wait-for-GPS row is the file's `[gps] wait-for-fix`, which `run` reads at launch. A run
//! decides at launch whether it waits, so the row applies from the next launch. The
//! waiting-for-GPS pop-up's box writes the same key, through [`save_fix_wait`].
//!
//! Every write to `wartui.toml` goes through [`save`], which also writes the bridge the
//! transport holds in [`Settings::bridge_memory`] and whether it remembers one. The view is
//! the file's only writer within `run`, so the transport's thread never races it on a
//! whole-file save. `flash-bridge` is the one other writer, and runs as its own command.

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
use wartui_proto::fan::FanDuty;
use wartui_proto::fleet::ScanMode;
use wartui_proto::link::SdState;
use wartui_proto::plan::ChannelPool;

use super::UploadTarget;
use super::centered_rect;
use super::ui::ENGINE_BUSY;
use crate::config;
use crate::text::{SdForm, sd_text};

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
    /// The file as last loaded or saved. A remembered bridge rewrites only the bridge from
    /// this copy.
    pub saved: config::Config,
    /// The bridge `run` remembers, and whether it remembers one. Shared with the
    /// transport, so the remember-bridge switch takes effect on the next reconnect and
    /// the bridge the transport connects to reaches the file.
    pub bridge_memory: BridgeMemory,
    /// Whether the link is the simulator, whose bridge is not a board to remember.
    pub simulated: bool,
    /// The link this run reaches its bridge over, saved as `[bridge] link` once it connects;
    /// `None` under the simulator, which saves no link.
    pub link: Option<config::Link>,
    /// Whether a bridge has announced itself over [`Self::link`]. Until then a save keeps
    /// `[bridge] link` as the file had it.
    pub link_proved: bool,
    /// Where `u` uploads to.
    pub upload: UploadTarget,
}

/// One row of the settings modal. A new setting adds a variant here and a row in
/// [`draw_settings_modal`]. The API key rows take typed text, through
/// [`ConfigModal::text_row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Pool,
    Fan,
    Bluetooth,
    WardriveEsp,
    RememberBridge,
    WaitForGps,
    ExcludedSsids,
    WdgwarsKey,
    WigleName,
    WigleToken,
    SdLog,
    SdCard,
}

/// Every row of the settings modal, top to bottom, in the order `next` and `prev` walk.
const FIELDS: [Field; 12] = [
    Field::Pool,
    Field::Fan,
    Field::Bluetooth,
    Field::WardriveEsp,
    Field::RememberBridge,
    Field::WaitForGps,
    Field::ExcludedSsids,
    Field::WdgwarsKey,
    Field::WigleName,
    Field::WigleToken,
    Field::SdLog,
    Field::SdCard,
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
/// edited independently of it. The format row holds nothing: it reads the live snapshot.
#[derive(Debug, Clone)]
pub(super) struct ConfigModal {
    selected: Field,
    /// The pool row's current value, edited by `step`, or `None` while no bridge has reported a
    /// pool.
    pool: Option<ChannelPool>,
    /// The pool the bridge reported when the modal opened, so `Enter` sends only a change.
    pool_seed: Option<ChannelPool>,
    /// The fan row's duty, or `None` while no bridge has reported a fan.
    fan: Option<FanDuty>,
    /// The duty the bridge reported when the modal opened, so `Enter` sends only a change.
    fan_seed: Option<FanDuty>,
    /// Whether the bluetooth row asks for the Bluetooth scan, or `None` while the bridge reports
    /// the scan stopped.
    bluetooth: Option<bool>,
    /// The bluetooth row as the modal opened, so `Enter` sends only a change.
    bluetooth_seed: Option<bool>,
    /// Whether the bridge lets a phone link as a WardriveESP board, or `None` while no bridge has
    /// reported it.
    wardrive_esp: Option<bool>,
    /// The wardrive esp row as the bridge reported it when the modal opened, so `Enter` sends
    /// only a change.
    wardrive_esp_seed: Option<bool>,
    /// Whether the bridge logs to its microSD card, or `None` while no bridge with a card slot has
    /// reported it.
    sd_log: Option<bool>,
    /// The log row as the bridge reported it when the modal opened, so `Enter` sends only a
    /// change.
    sd_log_seed: Option<bool>,
    /// Whether the link was up when the modal opened. While it is down, the pool, fan, wardrive
    /// esp and log rows are `None`: a value sent then would be dropped, not stored.
    bridge_up: bool,
    /// Whether `run` remembers the bridge it connected to.
    remember_bridge: bool,
    /// Whether `run` holds the scan for a GPS fix, from the next launch.
    wait_for_gps: bool,
    /// The WDGWars API key, typed or pasted. Empty means not set.
    wdgwars_key: String,
    /// WiGLE's API Name, typed or pasted. Empty means not set.
    wigle_name: String,
    /// WiGLE's API Token, typed or pasted. Empty means not set.
    wigle_token: String,
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

/// How far one step moves the fan row, in percent.
const FAN_STEP: u8 = 5;

/// Step `current` by [`FAN_STEP`] in `delta`'s direction, stopping at 0 and 100.
fn step_fan(current: FanDuty, delta: i8) -> FanDuty {
    let pct = match delta.signum() {
        1 => current.get().saturating_add(FAN_STEP).min(100),
        -1 => current.get().saturating_sub(FAN_STEP),
        _ => current.get(),
    };
    FanDuty::new(pct).unwrap_or(current)
}

/// `All → Eu → Us`, the order the pool row steps through.
const POOL_STEPS: [ChannelPool; 3] = [ChannelPool::All, ChannelPool::Eu, ChannelPool::Us];

/// Step `current` one place through [`POOL_STEPS`], wrapping past either end.
fn step_pool(current: ChannelPool, delta: i8) -> ChannelPool {
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
    /// Step the selected row by one. The pool row wraps past its ends, and the fan row stops at
    /// them.
    ///
    /// | Row | Step |
    /// |---|---|
    /// | pool | one place in [`POOL_STEPS`]; none at `no bridge` |
    /// | fan | [`FAN_STEP`], stopping at 0 and 100; none at `no fan` |
    /// | bluetooth | flips; none at `scan stopped` |
    /// | wardrive esp | flips; none at `no bridge` |
    /// | log | flips; none at `no bridge` or `no slot` |
    /// | remember | flips |
    /// | wait for gps | flips |
    /// | excluded ssids | none; [`Self::on_key`] opens its page |
    /// | wdgwars, wigle name, wigle token | none; [`Self::on_key`] types into it |
    /// | format | none; `Enter` asks to format the card ([`sd_row`]) |
    fn step(&mut self, delta: i8) {
        match self.selected {
            Field::Pool => self.pool = self.pool.map(|pool| step_pool(pool, delta)),
            Field::Fan => self.fan = self.fan.map(|duty| step_fan(duty, delta)),
            Field::Bluetooth => {
                if delta != 0 {
                    self.bluetooth = self.bluetooth.map(|on| !on);
                }
            }
            Field::WardriveEsp => {
                if delta != 0 {
                    self.wardrive_esp = self.wardrive_esp.map(|on| !on);
                }
            }
            Field::SdLog => {
                if delta != 0 {
                    self.sd_log = self.sd_log.map(|on| !on);
                }
            }
            Field::RememberBridge => {
                if delta != 0 {
                    self.remember_bridge = !self.remember_bridge;
                }
            }
            Field::WaitForGps => {
                if delta != 0 {
                    self.wait_for_gps = !self.wait_for_gps;
                }
            }
            // Text rows, a row that opens a page, and an action. `on_key` handles all three.
            Field::WdgwarsKey
            | Field::WigleName
            | Field::WigleToken
            | Field::ExcludedSsids
            | Field::SdCard => {}
        }
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
    /// `Enter` on the sd card row with a card to format: ask to confirm, and stay open.
    FormatSd,
}

impl ConfigModal {
    /// Open the modal, seeded from what is in force.
    pub(super) fn open(snapshot: &Snapshot, settings: &Settings) -> Self {
        // The bridge holds these four, so they are what its last `Status` said, and nothing
        // while the link is down.
        let bridge_up = snapshot.link_up;
        let pool = snapshot.pool.filter(|_| bridge_up);
        let wardrive_esp = snapshot.wardrive_esp.filter(|_| bridge_up);
        let sd_log = snapshot.sd_log.filter(|_| bridge_up);
        let fan = snapshot
            .bridge_status
            .and_then(|status| status.fan)
            .map(|fan| fan.duty)
            .filter(|_| bridge_up);
        // Nothing to toggle while stopped: the run starts the scan once the bridge reports it
        // stopped. Before the first report it is off, and the engine starts the scan in the mode
        // asked for meanwhile.
        let bluetooth = match snapshot.scan {
            Some(ScanMode::Stopped) => None,
            scan => Some(scan == Some(ScanMode::WifiBluetooth)),
        };
        Self {
            selected: Field::Pool,
            pool,
            pool_seed: pool,
            fan,
            fan_seed: fan,
            bluetooth,
            bluetooth_seed: bluetooth,
            wardrive_esp,
            wardrive_esp_seed: wardrive_esp,
            sd_log,
            sd_log_seed: sd_log,
            bridge_up,
            // The engine does not hold this one, so it comes from the handle.
            remember_bridge: settings.bridge_memory.is_enabled(),
            // The file's, since a run reads it only at launch.
            wait_for_gps: settings.saved.gps.waits_for_fix(),
            // Nor these. They are what the file holds, hand edits included.
            wdgwars_key: settings.saved.api_keys.wdgwars.clone(),
            wigle_name: settings.saved.api_keys.wigle_name.clone(),
            wigle_token: settings.saved.api_keys.wigle_token.clone(),
            excluded: snapshot.excluded_ssids.clone(),
            page: Page::Main,
        }
    }

    /// The selected row's text, when it is one of the API key rows.
    fn text_row(&mut self) -> Option<&mut String> {
        match self.selected {
            Field::WdgwarsKey => Some(&mut self.wdgwars_key),
            Field::WigleName => Some(&mut self.wigle_name),
            Field::WigleToken => Some(&mut self.wigle_token),
            Field::Pool
            | Field::Fan
            | Field::Bluetooth
            | Field::WardriveEsp
            | Field::SdLog
            | Field::RememberBridge
            | Field::WaitForGps
            | Field::ExcludedSsids
            | Field::SdCard => None,
        }
    }

    /// A key while the modal is open. `snapshot` is what the sd card row reads.
    pub(super) fn on_key(&mut self, key: KeyEvent, snapshot: &Snapshot) -> ModalAction {
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
        // The key rows are text fields. Letters type there rather than move or close.
        if let Some(text) = self.text_row() {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('u') if control => {
                    text.clear();
                    return ModalAction::Stay;
                }
                KeyCode::Char(c) if !control => {
                    text.push(c);
                    return ModalAction::Stay;
                }
                KeyCode::Backspace => {
                    text.pop();
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
            // An action, not a setting: `Enter` asks, or does nothing where the tip says why.
            KeyCode::Enter if self.selected == Field::SdCard => {
                return match sd_row(snapshot).1 {
                    Ok(()) => ModalAction::FormatSd,
                    Err(_) => ModalAction::Stay,
                };
            }
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

    /// A bracketed paste. Appended to the selected API key row, or to the SSID being
    /// typed on the excluded-SSIDs page, and ignored anywhere else. Control characters
    /// are dropped from both, so a trailing newline never reaches the file. The key rows
    /// drop whitespace too, which an SSID may hold.
    pub(super) fn paste(&mut self, text: &str) {
        if let Page::Excluded { input: Some(input), error, .. } = &mut self.page {
            input.extend(text.chars().filter(|c| !c.is_control()));
            *error = None;
            return;
        }
        if self.page != Page::Main {
            return;
        }
        let Some(row) = self.text_row() else { return };
        row.extend(text.chars().filter(|c| !c.is_whitespace() && !c.is_control()));
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
    // The bridge's rows go only when they moved: the bridge stores every pool, duty, wardrive esp
    // and log setting it is sent, and an unmoved bluetooth row would restate the scan.
    let pool = modal.pool.filter(|_| modal.pool != modal.pool_seed);
    let fan = modal.fan.filter(|_| modal.fan != modal.fan_seed);
    let bluetooth = modal.bluetooth.filter(|_| modal.bluetooth != modal.bluetooth_seed);
    let wardrive_esp = modal.wardrive_esp.filter(|_| modal.wardrive_esp != modal.wardrive_esp_seed);
    let sd_log = modal.sd_log.filter(|_| modal.sd_log != modal.sd_log_seed);
    let wanted = usize::from(pool.is_some())
        + usize::from(fan.is_some())
        + usize::from(bluetooth.is_some())
        + usize::from(wardrive_esp.is_some())
        + usize::from(sd_log.is_some());
    let Ok(mut permits) = commands.try_reserve_many(wanted) else {
        return Err(ENGINE_BUSY);
    };
    let pool_command = pool.map(|pool| Command::SetPool { pool });
    let fan_command = fan.map(|duty| Command::SetFanDuty { duty });
    let bluetooth_command = bluetooth.map(Command::SetBluetooth);
    let wardrive_esp_command = wardrive_esp.map(Command::SetWardriveEsp);
    let sd_log_command = sd_log.map(Command::SetSdLog);
    for command in pool_command
        .into_iter()
        .chain(fan_command)
        .chain(bluetooth_command)
        .chain(wardrive_esp_command)
        .chain(sd_log_command)
    {
        if let Some(permit) = permits.next() {
            permit.send(command);
        }
    }
    let mut text = match pool {
        Some(pool) => format!("pool {pool} sent to the bridge"),
        None => "settings applied".to_owned(),
    };
    if let Some(duty) = fan {
        text.push_str(&format!("; fan {duty} sent to the bridge"));
    }
    if let Some(on) = bluetooth {
        text.push_str(if on { "; bluetooth on" } else { "; bluetooth off" });
    }
    if let Some(on) = wardrive_esp {
        text.push_str(if on {
            "; wardrive esp on sent to the bridge, open to any phone in range"
        } else {
            "; wardrive esp off sent to the bridge"
        });
    }
    if let Some(on) = sd_log {
        text.push_str(if on {
            "; sd log on sent to the bridge"
        } else {
            "; sd log off sent to the bridge"
        });
    }
    let memory = &settings.bridge_memory;
    if modal.remember_bridge != memory.is_enabled() {
        memory.set_enabled(modal.remember_bridge);
        if modal.remember_bridge {
            // Remember the bridge connected now. A reconnect may never come.
            if let Some(bridge) = &snapshot.bridge
                && !settings.simulated
            {
                memory.remember_on(bridge.mac, None);
            }
            text.push_str("; bridge remembered");
        } else {
            text.push_str("; bridge forgotten — each start scans for it");
        }
    }
    // The row lives only in the file, so it changes nothing unless the save lands.
    let wait_moved = modal.wait_for_gps != settings.saved.gps.waits_for_fix();
    match save_outcome(modal, settings) {
        Ok(path) => {
            if wait_moved {
                text.push_str("; gps wait from the next launch");
            }
            text.push_str(&format!("; saved to {}", path.display()));
        }
        Err(error) => text.push_str(&error),
    }
    Ok(text)
}

/// `[gps] wait-for-fix` as written for `wait`: absent to wait, `false` to skip.
const fn wait_for_fix_key(wait: bool) -> Option<bool> {
    if wait { None } else { Some(false) }
}

/// Save whether `run` waits for a GPS fix at launch, over the file as last saved, for the
/// waiting-for-GPS pop-up's box. Returns the path, or why nothing was written, as [`save`]
/// does. Nothing else in the file changes.
pub(super) fn save_fix_wait(settings: &mut Settings, wait: bool) -> Result<PathBuf, String> {
    let mut written = settings.saved.clone();
    written.gps.wait_for_fix = wait_for_fix_key(wait);
    save(settings, written)
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
    let ssids = config::excluded_ssid_bytes(&modal.excluded).collect();
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

/// Write every modal row to `wartui.toml`. Returns the path, or the notice suffix saying
/// why nothing was written, as [`save`] does.
///
/// Overwrites the whole file, hand edits included. The rows are already applied, so a
/// failed save loses only the file. On success the written config becomes
/// [`Settings::saved`].
fn save_outcome(modal: &ConfigModal, settings: &mut Settings) -> Result<PathBuf, String> {
    // A row left where the file put it keeps the file's spelling, a hand-written `true`
    // included; only a moved row is rewritten.
    let wait_for_fix = if modal.wait_for_gps == settings.saved.gps.waits_for_fix() {
        settings.saved.gps.wait_for_fix
    } else {
        wait_for_fix_key(modal.wait_for_gps)
    };
    let written = config::Config {
        excluded_ssids: modal.excluded.clone(),
        bridge: config::Bridge {
            // Not on the modal, so kept as the file last had it, unless the run saves its own.
            link: settings.saved.bridge.link,
            remember: Some(modal.remember_bridge),
            ..config::Bridge::default()
        },
        gps: config::Gps {
            // Not on the modal, so kept as the file last had it.
            bridge_rate_hz: settings.saved.gps.bridge_rate_hz,
            wait_for_fix,
        },
        api_keys: config::ApiKeys {
            wdgwars: modal.wdgwars_key.trim().to_owned(),
            wigle_name: modal.wigle_name.trim().to_owned(),
            wigle_token: modal.wigle_token.trim().to_owned(),
        },
    };
    save(settings, written)
}

/// Save the bridge the transport holds now, over the file as last saved. Returns the
/// path, or why nothing was written, as [`save`] does.
pub(super) fn save_bridge(settings: &mut Settings) -> Result<PathBuf, String> {
    save(settings, settings.saved.clone())
}

/// Write `written` to the config path, with the bridge [`Settings::bridge_memory`] holds,
/// and on success make it [`Settings::saved`]. Returns the path, or the notice suffix
/// saying why nothing was written.
fn save(settings: &mut Settings, mut written: config::Config) -> Result<PathBuf, String> {
    let Some(path) = settings.config_path.clone() else {
        return Err("; nowhere to save it — use --config".to_owned());
    };
    // Nothing while remembering is off, so the keys drop out beside `remember = false`.
    written.bridge.mac = settings.bridge_memory.recall();
    written.bridge.serial = settings.bridge_memory.recall_serial();
    written.bridge.bluetooth_address = settings.bridge_memory.recall_bluetooth();
    if let Some(link) = settings.link.filter(|_| settings.link_proved) {
        written.bridge.link = link_written(link, written.bridge.link);
    }
    // The live switch, whatever an earlier failed save left in `saved`, so `remember` never
    // contradicts `mac`. An absent key stays absent while on, since absent means on.
    written.bridge.remember = match (settings.bridge_memory.is_enabled(), written.bridge.remember) {
        (true, None) => None,
        (on, _) => Some(on),
    };
    config::save(&path, &written).map_err(|error| format!("; could not save: {error}"))?;
    settings.saved = written;
    Ok(path)
}

/// What `[bridge] link` says once `link` is saved over `saved`: absent stays absent for USB,
/// since absent means USB.
pub(super) fn link_written(
    link: config::Link,
    saved: Option<config::Link>,
) -> Option<config::Link> {
    match (link, saved) {
        (config::Link::Usb, None) => None,
        (link, _) => Some(link),
    }
}

/// The tip on the sd card row when `Enter` asks to format the card.
const FORMAT_TIP: &str = "enter formats the card and erases it";

/// The sd card row as `snapshot` has it: its value, and `Ok` when `Enter` asks to format the
/// card, or the tip saying why it does nothing.
///
/// | Card | Value | `Enter` |
/// |---|---|---|
/// | link down | `no bridge` | nothing |
/// | no `Status` yet | `waiting` | nothing |
/// | none | `no slot` | nothing |
/// | no card | `no card` | nothing |
/// | not FAT32, ready, or failed | its size and state | asks |
/// | formatting | `formatting…` | nothing |
fn sd_row(snapshot: &Snapshot) -> (String, Result<(), &'static str>) {
    if !snapshot.link_up {
        return ("no bridge".to_owned(), Err("no bridge to format a card on · esc cancel"));
    }
    let Some(status) = snapshot.bridge_status else {
        return ("waiting".to_owned(), Err("waiting for the bridge's status · esc cancel"));
    };
    let sd = status.sd;
    let action = match sd.map(|sd| sd.state) {
        None => Err("this bridge has no card slot · esc cancel"),
        Some(SdState::NoCard) => Err("no card in the slot · esc cancel"),
        Some(SdState::Formatting) => Err("a format is running · esc cancel"),
        Some(SdState::NoFat32 | SdState::Ready | SdState::Failed) => Ok(()),
    };
    (sd_text(sd, SdForm::Row), action)
}

/// The settings modal, centered over the live view behind it. `snapshot` is the live one, which
/// the sd card row reads on every frame.
pub(super) fn draw_settings_modal(frame: &mut Frame<'_>, modal: &ConfigModal, snapshot: &Snapshot) {
    let area = centered_rect(48, 20, frame.area());
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
    let on_off = |on: bool| if on { "on" } else { "off" };
    let text_row = |label: &str, value: &str, field: Field| {
        Line::from(Span::styled(format!("{label:<18}  {value}"), styled(field)))
    };
    // What fits right of the 18-wide label column and its two spaces.
    let value_width = usize::from(inner.width).saturating_sub(20);
    const CLEAR_TIP: &str = "ctrl-u clear · enter save · esc cancel";
    let (sd_value, sd_action) = sd_row(snapshot);
    // A bridge that reported no card slot has no log to set.
    let no_slot =
        modal.bridge_up && snapshot.bridge_status.is_some_and(|status| status.sd.is_none());
    let lines = vec![
        match modal.pool {
            Some(pool) => row("pool", &pool, Field::Pool),
            // Nothing to step, so no arrows.
            None => {
                Line::from(Span::styled(format!("{:<18}  no bridge", "pool"), styled(Field::Pool)))
            }
        },
        match modal.fan {
            Some(duty) => row("fan", &duty, Field::Fan),
            // Nothing to step, so no arrows.
            None => {
                let why = if modal.bridge_up { "no fan" } else { "no bridge" };
                Line::from(Span::styled(format!("{:<18}  {why}", "fan"), styled(Field::Fan)))
            }
        },
        match modal.bluetooth {
            Some(on) => row("bluetooth", &on_off(on), Field::Bluetooth),
            // Nothing to step, so no arrows.
            None => Line::from(Span::styled(
                format!("{:<18}  scan stopped", "bluetooth"),
                styled(Field::Bluetooth),
            )),
        },
        match modal.wardrive_esp {
            Some(on) => row("wardrive esp", &on_off(on), Field::WardriveEsp),
            // Nothing to step, so no arrows.
            None => Line::from(Span::styled(
                format!("{:<18}  no bridge", "wardrive esp"),
                styled(Field::WardriveEsp),
            )),
        },
        row("remember bridge", &on_off(modal.remember_bridge), Field::RememberBridge),
        row("wait for gps", &on_off(modal.wait_for_gps), Field::WaitForGps),
        Line::from(Span::styled(
            format!("{:<18}  {} ›", "excluded ssids", excluded_count(&modal.excluded)),
            styled(Field::ExcludedSsids),
        )),
        Line::default(),
        Line::from("api keys"),
        text_row("wdgwars", &mask_key(&modal.wdgwars_key), Field::WdgwarsKey),
        text_row("wigle name", &fit_name(&modal.wigle_name, value_width), Field::WigleName),
        text_row("wigle token", &mask_key(&modal.wigle_token), Field::WigleToken),
        Line::default(),
        Line::from("sd card"),
        match modal.sd_log {
            Some(on) => row("log", &on_off(on), Field::SdLog),
            // Nothing to step, so no arrows.
            None => text_row("log", if no_slot { "no slot" } else { "no bridge" }, Field::SdLog),
        },
        // Nothing to step, so no arrows.
        text_row("format", &sd_value, Field::SdCard),
        Line::default(),
        // The one key a text row needs that nothing else on screen says.
        Line::from(match modal.selected {
            Field::WdgwarsKey if !modal.wdgwars_key.is_empty() => CLEAR_TIP,
            Field::WigleName if !modal.wigle_name.is_empty() => CLEAR_TIP,
            Field::WigleToken if !modal.wigle_token.is_empty() => CLEAR_TIP,
            Field::ExcludedSsids => "enter open · esc cancel",
            Field::SdCard => sd_action.err().unwrap_or(FORMAT_TIP),
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

/// What an empty API key row shows.
const NOT_SET: &str = "(not set, type or paste)";

/// The wigle name row's value: the whole name, or as much as fits in `width` and `…`.
/// WiGLE's account page shows the name openly, so the row does too.
fn fit_name(name: &str, width: usize) -> String {
    if name.is_empty() {
        return NOT_SET.to_owned();
    }
    if name.chars().count() <= width {
        return name.to_owned();
    }
    let mut shown: String = name.chars().take(width.saturating_sub(1)).collect();
    shown.push('…');
    shown
}

/// A secret as the wdgwars and wigle token rows show it, so a screen share or a photo
/// does not leak it.
///
/// | Length | Shown |
/// |---|---|
/// | 0 | [`NOT_SET`] |
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
        return NOT_SET.to_owned();
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

    use wartui_proto::link::SdStatus;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    #[test]
    fn ui_opens_modal_with_snapshot_values_when_c_key_is_pressed() {
        let mut snapshot = busy();
        snapshot.pool = Some(ChannelPool::Us);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let modal = ui.modal().expect("the modal opened");
        assert_eq!(modal.selected, Field::Pool);
        assert_eq!(modal.pool, Some(ChannelPool::Us));
        assert!(rx.try_recv().is_err(), "opening the modal sends nothing");
    }

    #[test]
    fn ui_seeds_pool_row_from_snapshot_pool_when_modal_opened() {
        let mut snapshot = busy();
        snapshot.pool = Some(ChannelPool::Eu);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        let modal = ui.modal().expect("the modal opened");
        assert_eq!(modal.pool, Some(ChannelPool::Eu));
    }

    #[test]
    fn ui_shows_no_bridge_and_steps_nothing_when_snapshot_pool_is_none() {
        let mut snapshot = busy();
        snapshot.pool = None;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").pool, None);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("pool                no bridge"), "{rendered}");

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);
        assert!(rx.try_recv().is_err(), "no pool to send");
    }

    #[test]
    fn ui_reaches_remember_bridge_row_from_pool_when_j_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").selected, Field::Fan);

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").selected, Field::Bluetooth);

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").selected, Field::WardriveEsp);

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(ui.modal().expect("still open").selected, Field::RememberBridge);
    }

    #[test]
    fn ui_steps_pool_and_wraps_at_ends_when_h_or_l_pressed() {
        let mut snapshot = busy();
        snapshot.pool = Some(ChannelPool::All);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("still open").selected, Field::Pool, "opens on the pool row");

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(
            ui.modal().expect("still open").pool,
            Some(ChannelPool::Us),
            "wraps from the start to the end"
        );

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(
            ui.modal().expect("still open").pool,
            Some(ChannelPool::All),
            "wraps from the end to the start"
        );

        for expected in [ChannelPool::Eu, ChannelPool::Us, ChannelPool::All].map(Some) {
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
    fn ui_sends_pool_command_when_enter_pressed_with_pool_changed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = busy();
        snapshot.pool = Some(ChannelPool::All);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetPool { pool: ChannelPool::Eu });
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.starts_with("pool EU sent to the bridge;"), "{notice}");
        let file = std::fs::read_to_string(&target).expect("the other rows were saved");
        assert!(!file.contains("pool"), "the bridge keeps it, not the file: {file}");
    }

    #[test]
    fn ui_sends_no_pool_when_enter_pressed_with_pool_row_unmoved() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        // Out and back: the pool the bridge already holds.
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "no SetPool");
    }

    #[test]
    fn ui_saves_all_rows_and_names_file_in_notice_when_enter_pressed_with_nothing_moved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let mut snapshot = busy();
        // The bridge's to keep, so nothing about it reaches the file.
        snapshot.scan = Some(wartui_proto::fleet::ScanMode::WifiBluetooth);
        snapshot.ble_slot = snapshot.nodes[0].state.slot;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "nothing moved, so nothing goes out");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains(&target.display().to_string()), "{notice}");
        let text = std::fs::read_to_string(&target).expect("saved");
        assert!(!text.contains("bluetooth"), "{text}");
    }

    #[test]
    fn ui_keeps_gps_rate_in_file_when_modal_saves() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let saved = config::Config {
            gps: config::Gps { bridge_rate_hz: Some(2), wait_for_fix: Some(false) },
            ..config::Config::default()
        };
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui::new(Settings { config_path: Some(target.clone()), saved, ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.gps.bridge_rate_hz, Some(2));
        assert_eq!(saved.gps.wait_for_fix, Some(false), "the row seeds from the file");
    }

    #[test]
    fn ui_keeps_hand_written_wait_for_fix_when_modal_saves_without_moving_row() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[gps]\nwait-for-fix = true\n").unwrap();
        let saved = config::load(Some(&target)).expect("a valid file");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui::new(Settings { config_path: Some(target.clone()), saved, ..Settings::default() });
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);
        // An unrelated row moves.
        while ui.modal().expect("open").selected != Field::RememberBridge {
            ui.on_modal_key(key(KeyCode::Down), &snapshot, &tx);
        }
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        let text = std::fs::read_to_string(&target).expect("saved");
        assert!(text.contains("wait-for-fix = true"), "{text}");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("gps wait"), "the row did not move: {notice}");
    }

    #[test]
    fn ui_says_only_why_not_saved_when_wait_for_gps_moves_with_nowhere_to_save() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: None, ..Settings::default() });
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);
        while ui.modal().expect("open").selected != Field::WaitForGps {
            ui.on_modal_key(key(KeyCode::Down), &snapshot, &tx);
        }
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("nowhere to save"), "{notice}");
        assert!(!notice.contains("gps wait from the next launch"), "{notice}");
    }

    #[test]
    fn ui_toggles_and_saves_wait_for_gps_when_row_stepped() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);
        while ui.modal().expect("open").selected != Field::WaitForGps {
            ui.on_modal_key(key(KeyCode::Down), &snapshot, &tx);
        }
        assert!(ui.modal().expect("open").wait_for_gps, "absent means wait");
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("wait for gps      ◂ on ▸"), "{rendered}");

        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "nothing goes to the engine");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("gps wait from the next launch"), "{notice}");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.gps.wait_for_fix, Some(false));

        // And back on: the key drops out of the file.
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);
        while ui.modal().expect("open").selected != Field::WaitForGps {
            ui.on_modal_key(key(KeyCode::Down), &snapshot, &tx);
        }
        assert!(!ui.modal().expect("open").wait_for_gps);
        ui.on_modal_key(key(KeyCode::Char('h')), &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);
        let text = std::fs::read_to_string(&target).expect("saved");
        assert!(!text.contains("wait-for-fix"), "{text}");
    }

    #[test]
    fn ui_sends_nothing_and_saves_nothing_when_enter_pressed_with_queue_one_slot_short() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = fan_snapshot(40);
        let (tx, mut rx) = mpsc::channel(3);
        tx.try_send(Command::StopScan).unwrap();
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        // The pool, fan and bluetooth rows moved, so the modal sends three commands.
        for row in [Field::Pool, Field::Fan, Field::Bluetooth] {
            assert_eq!(ui.modal().expect("open").selected, row);
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
            ui.on_modal_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &snapshot, &tx);
        }

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        // Room for two of the modal's three commands, so none goes out.
        assert_eq!(rx.try_recv().unwrap(), Command::StopScan);
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
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Command::StopScan).unwrap();
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        // Open with the edit, so the operator can press Enter again once the engine
        // takes commands.
        let modal = ui.modal().expect("still open");
        assert_eq!(modal.pool, Some(ChannelPool::Eu), "the edit survives");
        assert!(!target.exists(), "nothing was written");
        assert_eq!(ui.notice(snapshot.now_ms), Some(ENGINE_BUSY));
    }

    #[test]
    fn ui_sends_nothing_and_saves_nothing_when_enter_pressed_with_no_queue_slot_free() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(Command::StopScan).unwrap();
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::StopScan);
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        assert!(!target.exists(), "nothing was written");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("not accepting commands"), "{notice}");
    }

    #[test]
    fn ui_overwrites_hand_edit_made_mid_run_when_enter_pressed() {
        // Enter saves what the modal shows, not what the file holds. A hand edit made
        // while the modal is open is overwritten like anything else.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[bridge]\nremember = false\n").unwrap();
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        // Hand-edited while the modal is open: a different list and remember switch.
        let edit = "excluded-ssids = [\"Hand\"]\n[bridge]\nremember = false\n";
        std::fs::write(&target, edit).unwrap();
        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.excluded_ssids, snapshot.excluded_ssids, "the modal's list wins");
        assert_eq!(saved.bridge.remember, Some(true), "the modal's switch wins");
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
        // Only the remember row moves. The pool row is left alone.
        while ui.modal().expect("open").selected != Field::RememberBridge {
            ui.on_modal_key(key(KeyCode::Down), &snapshot, &tx);
        }
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.starts_with("settings applied;"), "{notice}");
        assert!(!notice.contains("pool"), "{notice}");
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
        // The bridge is sent the pool whether or not the file saves.
        assert_eq!(rx.try_recv().unwrap(), Command::SetPool { pool: ChannelPool::Eu });
    }

    /// A view saving to `dir/wartui.toml`, holding the bridge in `memory`.
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
        assert_eq!(modal.selected, Field::RememberBridge, "the row under the fan");
        assert!(modal.remember_bridge, "seeded from the handle, on by default");

        for (key, expected) in [('h', false), ('h', true), ('l', false), ('l', true)] {
            ui.on_modal_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE), &snapshot, &tx);
            assert_eq!(ui.modal().expect("still open").remember_bridge, expected, "{key} flips it");
        }
    }

    /// The bridge saved in `dir/wartui.toml`, which must load.
    fn saved_bridge(dir: &std::path::Path) -> config::Bridge {
        config::load(Some(&dir.join("wartui.toml"))).expect("a valid file").bridge
    }

    const OTHER_BRIDGE: wartui_proto::mac::Mac = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

    #[test]
    fn ui_forgets_bridge_and_saves_off_when_remember_bridge_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::holding(Some(OTHER_BRIDGE));
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        select_remember_bridge(&mut ui, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(!memory.is_enabled(), "the transport's clone sees it too");
        assert_eq!(memory.recall(), None);
        let saved = saved_bridge(dir.path());
        assert_eq!(saved.remember, Some(false));
        assert_eq!(saved.mac, None, "off saves no bridge");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("bridge forgotten"), "{notice}");
    }

    #[test]
    fn ui_saves_connected_bridge_when_remember_bridge_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::none();
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
        assert_eq!(memory.recall(), Some(bridge), "the bridge connected now");
        let saved = saved_bridge(dir.path());
        assert_eq!(saved.remember, Some(true));
        assert_eq!(saved.mac, Some(bridge));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("bridge remembered"), "{notice}");
    }

    #[test]
    fn ui_keeps_remembered_bridge_when_remember_bridge_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        // A board other than the one connected. An unchanged row must not replace it.
        let memory = BridgeMemory::holding(Some(OTHER_BRIDGE));
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(memory.is_enabled());
        assert_eq!(memory.recall(), Some(OTHER_BRIDGE));
        assert_eq!(saved_bridge(dir.path()).mac, Some(OTHER_BRIDGE));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("bridge forgotten"), "{notice}");
        assert!(!notice.contains("bridge remembered"), "{notice}");
    }

    /// A view saving to `dir/wartui.toml` for a run over `link`.
    fn ui_over(dir: &std::path::Path, link: Option<config::Link>) -> Ui {
        Ui::new(Settings {
            config_path: Some(dir.join("wartui.toml")),
            link,
            ..Settings::default()
        })
    }

    #[test]
    fn ui_saves_link_when_bridge_announces_itself_over_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = ui_over(dir.path(), Some(config::Link::Bluetooth));
        let mut waiting = busy();
        waiting.bridge = None;
        ui.poll_link(&waiting);
        assert!(!dir.path().join("wartui.toml").exists(), "not before it connects");

        ui.poll_link(&busy());

        assert_eq!(saved_bridge(dir.path()).link, Some(config::Link::Bluetooth));
        assert_eq!(ui.saved().bridge.link, Some(config::Link::Bluetooth));
    }

    #[test]
    fn ui_keeps_link_as_file_had_it_when_settings_save_before_bridge_announces_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nlink = \"usb\"\n").unwrap();
        let mut settings = Settings {
            config_path: Some(path.clone()),
            saved: config::load(Some(&path)).unwrap(),
            link: Some(config::Link::Bluetooth),
            ..Settings::default()
        };

        save_fix_wait(&mut settings, false).unwrap();
        assert_eq!(saved_bridge(dir.path()).link, Some(config::Link::Usb));

        std::fs::write(&path, "").unwrap();
        settings.saved = config::load(Some(&path)).unwrap();
        save_fix_wait(&mut settings, true).unwrap();
        assert_eq!(saved_bridge(dir.path()).link, None, "an absent key stays absent");
    }

    #[test]
    fn ui_writes_nothing_when_run_is_over_usb_and_file_names_no_link() {
        // Absent means USB, so there is nothing to change.
        let dir = tempfile::tempdir().unwrap();
        let mut ui = ui_over(dir.path(), Some(config::Link::Usb));
        ui.poll_link(&busy());
        assert!(!dir.path().join("wartui.toml").exists());
    }

    #[test]
    fn ui_saves_usb_when_file_names_bluetooth_and_run_is_over_usb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nlink = \"bluetooth\"\n").unwrap();
        let mut ui = Ui::new(Settings {
            config_path: Some(path),
            saved: config::load(Some(&dir.path().join("wartui.toml"))).unwrap(),
            link: Some(config::Link::Usb),
            ..Settings::default()
        });

        ui.poll_link(&busy());

        assert_eq!(saved_bridge(dir.path()).link, Some(config::Link::Usb));
    }

    #[test]
    fn ui_saves_no_link_when_link_is_the_simulator() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = ui_over(dir.path(), None);
        ui.poll_link(&busy());
        assert!(!dir.path().join("wartui.toml").exists());
    }

    #[test]
    fn ui_saves_bluetooth_address_when_transport_remembers_one() {
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::none();
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        memory.remember_bluetooth(OTHER_BRIDGE, "02:00:5E:10:44:C1".to_owned());

        ui.poll_bridge();

        let saved = saved_bridge(dir.path());
        assert_eq!(saved.mac, Some(OTHER_BRIDGE));
        assert_eq!(saved.bluetooth_address.as_deref(), Some("02:00:5E:10:44:C1"));
    }

    #[test]
    fn ui_saves_bridge_when_transport_remembers_one() {
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::none();
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        ui.poll_bridge();
        assert!(!dir.path().join("wartui.toml").exists(), "nothing changed, nothing written");

        memory.remember_on(OTHER_BRIDGE, None); // As the transport does on connecting.
        ui.poll_bridge();

        assert_eq!(saved_bridge(dir.path()).mac, Some(OTHER_BRIDGE));
        assert_eq!(ui.saved().bridge.mac, Some(OTHER_BRIDGE), "the view knows it too");
        assert_eq!(ui.notice(busy().now_ms), None, "silent");
    }

    #[test]
    fn ui_saves_removal_when_transport_forgets_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::none();
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        memory.remember_on(OTHER_BRIDGE, None);
        ui.poll_bridge();

        memory.forget(); // As the transport does when the remembered board goes quiet.
        ui.poll_bridge();

        assert_eq!(saved_bridge(dir.path()).mac, None);
    }

    #[test]
    fn ui_saves_remember_on_with_bridge_when_saved_copy_still_says_off() {
        // As after turning remember on in the modal with a save that failed.
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::none();
        let saved = config::Config {
            bridge: config::Bridge { remember: Some(false), ..config::Bridge::default() },
            ..config::Config::default()
        };
        let mut ui = Ui::new(Settings {
            config_path: Some(dir.path().join("wartui.toml")),
            saved,
            bridge_memory: memory.clone(),
            ..Settings::default()
        });
        memory.remember_on(OTHER_BRIDGE, None);

        ui.poll_bridge();

        let file = saved_bridge(dir.path());
        assert_eq!(file.remember, Some(true));
        assert_eq!(file.mac, Some(OTHER_BRIDGE));
    }

    #[test]
    fn ui_saves_remember_off_without_bridge_when_saved_copy_still_says_on() {
        // As after turning remember off in the modal with a save that failed.
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::holding(Some(OTHER_BRIDGE));
        let saved = config::Config {
            bridge: config::Bridge {
                remember: Some(true),
                mac: Some(OTHER_BRIDGE),
                ..config::Bridge::default()
            },
            ..config::Config::default()
        };
        let mut ui = Ui::new(Settings {
            config_path: Some(dir.path().join("wartui.toml")),
            saved,
            bridge_memory: memory.clone(),
            ..Settings::default()
        });
        memory.set_enabled(false);

        ui.poll_bridge();

        let file = saved_bridge(dir.path());
        assert_eq!(file.remember, Some(false));
        assert_eq!(file.mac, None);
    }

    #[test]
    fn ui_saves_remember_on_without_bridge_when_link_is_simulated() {
        let dir = tempfile::tempdir().unwrap();
        let memory = BridgeMemory::none();
        memory.set_enabled(false);
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings {
            config_path: Some(dir.path().join("wartui.toml")),
            bridge_memory: memory.clone(),
            simulated: true,
            ..Settings::default()
        });
        select_remember_bridge(&mut ui, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert!(memory.is_enabled());
        assert_eq!(memory.recall(), None, "the simulator's bridge is not a board");
        let saved = saved_bridge(dir.path());
        assert_eq!(saved.remember, Some(true));
        assert_eq!(saved.mac, None);
    }

    #[test]
    fn ui_leaves_file_alone_when_remembered_bridge_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        let memory = BridgeMemory::none();
        let mut ui = ui_with_bridge_memory(dir.path(), &memory);
        memory.remember_on(OTHER_BRIDGE, None);
        ui.poll_bridge();
        // A hand edit after the save. Only a change to the bridge rewrites the file.
        std::fs::write(&path, "# mine\n").unwrap();

        ui.poll_bridge();
        memory.remember_on(OTHER_BRIDGE, None);
        ui.poll_bridge();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# mine\n");
    }

    #[test]
    #[cfg(unix)]
    fn ui_waits_for_next_change_when_bridge_save_fails() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the config's directory would have to go: every save fails.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let memory = BridgeMemory::none();
        let mut ui = ui_with_bridge_memory(&blocked, &memory);
        memory.remember_on(OTHER_BRIDGE, None);
        ui.poll_bridge();
        assert_eq!(ui.saved().bridge.mac, None, "the save failed");

        // Now the save would succeed, but the bridge has not changed since it failed.
        std::fs::remove_file(&blocked).unwrap();
        ui.poll_bridge();
        assert!(!blocked.exists(), "not retried every frame");

        let bridge = busy().bridge.expect("a bridge").mac;
        memory.remember_on(bridge, None);
        ui.poll_bridge();
        assert_eq!(saved_bridge(&blocked).mac, Some(bridge), "a new bridge is tried again");
    }

    #[test]
    #[cfg(unix)]
    fn ui_saves_bridge_when_it_returns_after_a_failed_save() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let memory = BridgeMemory::none();
        let mut ui = ui_with_bridge_memory(&blocked, &memory);
        memory.remember_on(OTHER_BRIDGE, None);
        ui.poll_bridge();
        assert_eq!(ui.saved().bridge.mac, None, "the save failed");

        // Back to what the file holds, which clears the failed attempt.
        memory.forget();
        ui.poll_bridge();
        std::fs::remove_file(&blocked).unwrap();
        memory.remember_on(OTHER_BRIDGE, None);
        ui.poll_bridge();

        assert_eq!(saved_bridge(&blocked).mac, Some(OTHER_BRIDGE));
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
        assert!(rendered.contains("pool"), "{rendered}");
        assert!(!rendered.contains("remember bt node"), "the bridge keeps that: {rendered}");
        assert!(!rendered.contains("tx power"), "{rendered}");
        assert!(rendered.contains("remember bridge"), "{rendered}");
        // The view behind it is still live, not blanked out.
        assert!(rendered.contains(" N7 "), "{rendered}");
    }

    /// Open the modal and walk down to the `wdgwars` row.
    fn select_wdgwars(ui: &mut Ui, snapshot: &Snapshot, tx: &mpsc::Sender<Command>) {
        select(Field::WdgwarsKey, ui, snapshot, tx);
    }

    fn press(ui: &mut Ui, code: KeyCode, modifiers: KeyModifiers, snapshot: &Snapshot) {
        let (tx, _rx) = mpsc::channel(4);
        ui.on_modal_key(KeyEvent::new(code, modifiers), snapshot, &tx);
    }

    /// The rows that take typed text.
    const TEXT_ROWS: [Field; 3] = [Field::WdgwarsKey, Field::WigleName, Field::WigleToken];

    /// What the modal holds for the text row `field`.
    fn text_of(modal: &ConfigModal, field: Field) -> &str {
        match field {
            Field::WdgwarsKey => &modal.wdgwars_key,
            Field::WigleName => &modal.wigle_name,
            Field::WigleToken => &modal.wigle_token,
            other => panic!("{other:?} is not a text row"),
        }
    }

    #[test]
    fn ui_types_letters_into_row_when_text_row_selected() {
        let snapshot = busy();
        for field in TEXT_ROWS {
            let (tx, mut rx) = mpsc::channel(4);
            let mut ui = Ui::default();
            select(field, &mut ui, &snapshot, &tx);

            for c in "hjklq".chars() {
                press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE, &snapshot);
            }

            let modal = ui.modal().expect("q types rather than closes");
            assert_eq!(modal.selected, field, "j and k type rather than move");
            assert_eq!(text_of(modal, field), "hjklq", "{field:?}");
            for other in TEXT_ROWS.into_iter().filter(|&other| other != field) {
                assert_eq!(text_of(modal, other), "", "{field:?} typed into {other:?}");
            }
            assert!(rx.try_recv().is_err(), "nothing was sent");
        }
    }

    #[test]
    fn ui_edits_row_when_backspace_or_ctrl_u_pressed_on_text_row() {
        let snapshot = busy();
        for field in TEXT_ROWS {
            let (tx, _rx) = mpsc::channel(4);
            let mut ui = Ui::default();
            select(field, &mut ui, &snapshot, &tx);
            for c in "abc".chars() {
                press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE, &snapshot);
            }

            press(&mut ui, KeyCode::Backspace, KeyModifiers::NONE, &snapshot);
            assert_eq!(text_of(ui.modal().expect("open"), field), "ab", "{field:?}");

            press(&mut ui, KeyCode::Char('x'), KeyModifiers::CONTROL, &snapshot);
            assert_eq!(text_of(ui.modal().expect("open"), field), "ab", "ctrl-x types nothing");

            press(&mut ui, KeyCode::Char('u'), KeyModifiers::CONTROL, &snapshot);
            assert_eq!(text_of(ui.modal().expect("open"), field), "", "{field:?}");
        }
    }

    #[test]
    fn ui_reaches_wigle_and_sd_rows_and_stops_when_down_pressed_from_wdgwars_row() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select_wdgwars(&mut ui, &snapshot, &tx);
        assert_eq!(FIELDS.last(), Some(&Field::SdCard), "the last row");

        let walk =
            [Field::WigleName, Field::WigleToken, Field::SdLog, Field::SdCard, Field::SdCard];
        for expected in walk {
            press(&mut ui, KeyCode::Down, KeyModifiers::NONE, &snapshot);
            assert_eq!(ui.modal().expect("open").selected, expected);
        }
        press(&mut ui, KeyCode::Up, KeyModifiers::NONE, &snapshot);
        assert_eq!(ui.modal().expect("open").selected, Field::SdLog);
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
    fn ui_appends_paste_without_whitespace_when_text_row_selected() {
        let snapshot = busy();
        for field in TEXT_ROWS {
            let (tx, _rx) = mpsc::channel(4);
            let mut ui = Ui::default();
            select(field, &mut ui, &snapshot, &tx);
            press(&mut ui, KeyCode::Char('x'), KeyModifiers::NONE, &snapshot);

            ui.on_modal_paste(" abc\tdef\u{7}\r\n");

            let modal = ui.modal().expect("a pasted newline does not save");
            assert_eq!(text_of(modal, field), "xabcdef", "{field:?}");
        }
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
        for field in TEXT_ROWS {
            assert_eq!(text_of(modal, field), "", "{field:?}");
        }
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
    fn ui_saves_wigle_name_and_token_that_reload_when_enter_pressed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        select(Field::WigleName, &mut ui, &snapshot, &tx);
        for c in "AIDname ".chars() {
            press(&mut ui, KeyCode::Char(c), KeyModifiers::NONE, &snapshot);
        }
        press(&mut ui, KeyCode::Down, KeyModifiers::NONE, &snapshot);
        ui.on_modal_paste("exampletoken789");

        press(&mut ui, KeyCode::Enter, KeyModifiers::NONE, &snapshot);

        assert!(ui.modal().is_none(), "Enter saves and closes on the token row too");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.api_keys.wigle_name, "AIDname", "trimmed");
        assert_eq!(saved.api_keys.wigle_token, "exampletoken789");
        assert_eq!(saved.api_keys.wdgwars, "");
        assert_eq!(ui.saved().api_keys.wigle_token, "exampletoken789", "the view knows it too");

        // And the next opening shows what the file holds.
        ui.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &snapshot, &tx);
        let modal = ui.modal().expect("open");
        assert_eq!(modal.wigle_name, "AIDname");
        assert_eq!(modal.wigle_token, "exampletoken789");
    }

    #[test]
    fn ui_keeps_hand_written_keys_when_enter_pressed_without_touching_them() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let text = "[api-keys]\nwdgwars = \"by-hand-key\"\nwigle-name = \"hand-name\"\n\
                    wigle-token = \"hand-token\"\n";
        std::fs::write(&target, text).unwrap();
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
        assert_eq!(file.api_keys.wigle_name, "hand-name");
        assert_eq!(file.api_keys.wigle_token, "hand-token");
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
            api_keys: config::ApiKeys {
                wdgwars: "abcSECRETxyz".to_owned(),
                ..config::ApiKeys::default()
            },
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
        let keys = config::ApiKeys { wdgwars: key.to_owned(), ..config::ApiKeys::default() };
        modal_terminal(keys, field).backend().to_string()
    }

    /// The terminal at 120×30 with the modal open, `keys` saved and the cursor on `field`.
    fn modal_terminal(keys: config::ApiKeys, field: Field) -> Terminal<TestBackend> {
        let snapshot = busy();
        let saved = config::Config { api_keys: keys, ..config::Config::default() };
        let mut ui = Ui::new(Settings { saved, ..Settings::default() });
        let (tx, _rx) = mpsc::channel(4);
        select(field, &mut ui, &snapshot, &tx);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        terminal
    }

    /// `keys` holding only WiGLE's name and token.
    fn wigle_keys(name: &str, token: &str) -> config::ApiKeys {
        config::ApiKeys {
            wigle_name: name.to_owned(),
            wigle_token: token.to_owned(),
            ..config::ApiKeys::default()
        }
    }

    #[test]
    fn draw_shows_wigle_name_in_full_and_token_masked_when_modal_open() {
        let keys = wigle_keys("AIDexamplename", "tokSECRETend");
        let rendered = modal_terminal(keys, Field::Pool).backend().to_string();

        assert!(rendered.contains("wigle name          AIDexamplename"), "{rendered}");
        assert!(rendered.contains("wigle token         tok••••••end"), "{rendered}");
        assert!(!rendered.contains("SECRET"), "{rendered}");
    }

    #[test]
    fn draw_says_not_set_on_wigle_rows_when_both_are_empty() {
        let rendered = modal_rendered("", Field::Pool);
        assert!(rendered.contains("wigle name          (not set, type or paste)"), "{rendered}");
        assert!(rendered.contains("wigle token         (not set, type or paste)"), "{rendered}");
    }

    #[test]
    fn draw_truncates_wigle_name_and_keeps_border_when_name_is_wider_than_its_column() {
        // Shaped like a WiGLE API name: `AID` and 32 hex digits, made up.
        let name = format!("AID{}", "0123456789abcdef".repeat(2));
        let terminal = modal_terminal(wigle_keys(&name, ""), Field::WigleName);
        let rendered = terminal.backend().to_string();

        assert!(!rendered.contains(&name), "{rendered}");
        // 46 columns inside the border, less the 20 of the label column.
        assert!(rendered.contains(&format!("{}…", &name[..25])), "{rendered}");
        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.height)
            .find(|&y| {
                let line: String =
                    (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect();
                line.contains("wigle name")
            })
            .expect("the name row");
        // The modal is 48 wide, centered in 120: columns 36 to 83.
        assert_eq!(buffer[(36, row)].symbol(), "│", "{rendered}");
        assert_eq!(buffer[(83, row)].symbol(), "│", "{rendered}");
    }

    #[test]
    fn draw_shows_enter_save_when_wigle_token_row_selected() {
        let rendered = modal_terminal(wigle_keys("", ""), Field::WigleToken).backend().to_string();
        assert!(rendered.contains("wigle token"), "{rendered}");
        assert!(
            rendered.contains("enter save · esc cancel"),
            "the modal is tall enough: {rendered}"
        );
    }

    #[test]
    fn draw_shows_ctrl_u_tip_when_wigle_row_selected_with_text_entered() {
        for field in [Field::WigleName, Field::WigleToken] {
            let rendered =
                modal_terminal(wigle_keys("AIDname", "tok"), field).backend().to_string();
            assert!(rendered.contains("ctrl-u clear · enter save · esc cancel"), "{rendered}");
        }
    }

    #[test]
    fn draw_omits_ctrl_u_tip_when_wigle_row_selected_with_text_empty() {
        for field in [Field::WigleName, Field::WigleToken] {
            let rendered = modal_terminal(wigle_keys("", ""), field).backend().to_string();
            assert!(!rendered.contains("ctrl-u"), "{field:?}: {rendered}");
        }
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
        let snapshot = excluded_snapshot(&["HomeNet"]);
        let saved = config::Config {
            gps: config::Gps { bridge_rate_hz: Some(2), wait_for_fix: None },
            ..config::Config::default()
        };
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
        assert_eq!(file.gps.bridge_rate_hz, Some(2), "the rest of the file is as `saved` held it");
        let modal = ui.modal().expect("the modal stays open");
        assert_eq!(modal.page, Page::Main);
        assert_eq!(modal.selected, Field::ExcludedSsids);
        assert_eq!(modal.pool, Some(ChannelPool::Eu), "the pool edit stays pending");
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
        tx.try_send(Command::StopScan).unwrap();

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::StopScan);
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
    fn excluded_page_lines_keeps_cursor_in_view_when_list_is_longer_than_page() {
        let names: Vec<String> = (0..20).map(|n| format!("net-{n:02}")).collect();
        let lines = excluded_page_lines(&names, 19, None, None, 12);

        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(text.len(), 12, "fills the page exactly");
        assert_eq!(text[8], "net-19", "the cursor's row is the last one listed");
        assert_eq!(text[0], "net-11");
    }

    /// `busy()` with the bridge reporting the fan at `pct`.
    fn fan_snapshot(pct: u8) -> Snapshot {
        let mut snapshot = busy();
        let duty = FanDuty::new(pct).expect("in range");
        if let Some(status) = &mut snapshot.bridge_status {
            status.fan = Some(wartui_proto::fan::FanStats { duty, rpm: Some(1830) });
        }
        snapshot
    }

    fn duty(pct: u8) -> FanDuty {
        FanDuty::new(pct).expect("in range")
    }

    #[test]
    fn ui_seeds_fan_row_from_bridge_status_when_modal_opened() {
        let snapshot = fan_snapshot(40);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Fan, &mut ui, &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").fan, Some(duty(40)));

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("fan               ◂ 40% ▸"), "{rendered}");
    }

    #[test]
    fn ui_steps_fan_by_five_and_stops_at_ends_when_h_or_l_pressed() {
        let snapshot = fan_snapshot(95);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Fan, &mut ui, &snapshot, &tx);
        let fan = |ui: &Ui| ui.modal().expect("open").fan;

        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(fan(&ui), Some(FanDuty::FULL));
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(fan(&ui), Some(FanDuty::FULL), "stops at 100 rather than wrapping");
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(fan(&ui), Some(duty(95)));

        assert_eq!(step_fan(duty(3), -1), duty(0), "stops at 0");
        assert_eq!(step_fan(duty(0), -1), duty(0));
        assert_eq!(step_fan(duty(42), 1), duty(47));
    }

    #[test]
    fn ui_shows_no_fan_and_steps_nothing_when_bridge_reports_none() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Fan, &mut ui, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").fan, None);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("fan                 no fan"), "{rendered}");
    }

    #[test]
    fn ui_shows_no_bridge_on_both_bridge_rows_and_sends_neither_when_link_is_down() {
        let mut snapshot = fan_snapshot(40);
        snapshot.pool = Some(ChannelPool::Eu);
        snapshot.link_up = false;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        let right = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        select(Field::Pool, &mut ui, &snapshot, &tx);
        ui.on_modal_key(right, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").selected, Field::Fan);
        ui.on_modal_key(right, &snapshot, &tx);
        let modal = ui.modal().expect("open");
        assert_eq!((modal.pool, modal.fan), (None, None));

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("pool                no bridge"), "{rendered}");
        assert!(rendered.contains("fan                 no bridge"), "{rendered}");

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);
        let sent: Vec<Command> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !sent.iter().any(|c| matches!(c, Command::SetPool { .. } | Command::SetFanDuty { .. })),
            "{sent:?}"
        );
    }

    #[test]
    fn ui_sends_fan_duty_and_saves_none_of_it_when_enter_pressed_with_fan_moved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = fan_snapshot(40);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        select(Field::Fan, &mut ui, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetFanDuty { duty: duty(35) });
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("fan 35% sent to the bridge"), "{notice}");
        let file = std::fs::read_to_string(&target).expect("the other rows were saved");
        assert!(!file.contains("fan"), "the bridge keeps it, not the file: {file}");
    }

    #[test]
    fn ui_sends_no_fan_duty_when_enter_pressed_with_fan_row_unchanged() {
        let snapshot = fan_snapshot(40);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Fan, &mut ui, &snapshot, &tx);
        // Out and back: the duty the bridge already runs.
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        let sent: Vec<Command> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(!sent.iter().any(|cmd| matches!(cmd, Command::SetFanDuty { .. })), "{sent:?}");
    }

    #[test]
    fn ui_sends_no_fan_duty_when_esc_pressed_with_fan_moved() {
        let snapshot = fan_snapshot(40);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Fan, &mut ui, &snapshot, &tx);
        ui.on_modal_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snapshot, &tx);

        ui.on_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "Esc leaves the bridge's duty alone");
    }

    #[test]
    fn ui_sends_wardrive_esp_when_row_moved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        select(Field::WardriveEsp, &mut ui, &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Right), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetWardriveEsp(true));
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(
            notice.contains("wardrive esp on sent to the bridge, open to any phone in range"),
            "{notice}"
        );
        let file = std::fs::read_to_string(&target).expect("the other rows were saved");
        assert!(!file.contains("wardrive"), "the bridge keeps it, not the file: {file}");
    }

    #[test]
    fn ui_sends_nothing_when_wardrive_esp_row_unmoved() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::WardriveEsp, &mut ui, &snapshot, &tx);
        // Out and back: the setting the bridge already holds.
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Char('h')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "no SetWardriveEsp");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("wardrive esp"), "{notice}");
    }

    #[test]
    fn ui_keeps_wardrive_esp_row_still_when_bridge_has_not_reported() {
        let snapshot = Snapshot { wardrive_esp: None, ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::WardriveEsp, &mut ui, &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").wardrive_esp, None);
        let screen = modal_screen(&snapshot, &mut ui);
        assert!(screen.contains("wardrive esp        no bridge"), "{screen}");

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "no SetWardriveEsp");
    }

    #[test]
    fn ui_draws_wardrive_esp_row_when_opened() {
        let snapshot = Snapshot { wardrive_esp: Some(true), ..busy() };
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);

        let screen = modal_screen(&snapshot, &mut ui);

        assert!(screen.contains("wardrive esp      ◂ on ▸"), "{screen}");
        // The modal grew a row, and the last still fits.
        assert!(screen.contains("enter save · esc cancel"), "{screen}");
    }

    #[test]
    fn ui_draws_sd_log_row_between_sd_card_heading_and_format_when_opened() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(key(KeyCode::Char('c')), &snapshot, &tx);

        let screen = modal_screen(&snapshot, &mut ui);

        let lines: Vec<&str> = screen.lines().collect();
        let at = |text: &str| lines.iter().position(|line| line.contains(text)).expect(text);
        let log = at("log               ◂ on ▸");
        assert_eq!(at("sd card"), log - 1, "{screen}");
        assert_eq!(at("format              "), log + 1, "{screen}");
        // The modal grew a row, and the last still fits.
        assert!(screen.contains("enter save · esc cancel"), "{screen}");
    }

    #[test]
    fn ui_toggles_sd_log_row_when_h_or_l_pressed() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdLog, &mut ui, &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").sd_log, Some(true));

        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").sd_log, Some(false));
        let screen = modal_screen(&snapshot, &mut ui);
        assert!(screen.contains("log               ◂ off ▸"), "{screen}");

        ui.on_modal_key(key(KeyCode::Char('h')), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").sd_log, Some(true));
    }

    #[test]
    fn ui_sends_sd_log_when_row_moved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        select(Field::SdLog, &mut ui, &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Right), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetSdLog(false));
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("sd log off sent to the bridge"), "{notice}");
        let file = std::fs::read_to_string(&target).expect("the other rows were saved");
        assert!(!file.contains("log"), "the bridge keeps it, not the file: {file}");
    }

    #[test]
    fn ui_sends_nothing_when_sd_log_row_unmoved() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdLog, &mut ui, &snapshot, &tx);
        // Out and back: the setting the bridge already holds.
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Char('h')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "no SetSdLog");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("sd log"), "{notice}");
    }

    #[test]
    fn ui_keeps_sd_log_row_still_when_bridge_is_down_or_has_no_slot() {
        let down = Snapshot { link_up: false, ..busy() };
        let no_slot = Snapshot { sd_log: None, ..sd_snapshot(None) };
        for (snapshot, value) in [(down, "no bridge"), (no_slot, "no slot")] {
            let (tx, mut rx) = mpsc::channel(4);
            let mut ui = Ui::default();
            select(Field::SdLog, &mut ui, &snapshot, &tx);
            ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
            assert_eq!(ui.modal().expect("open").sd_log, None, "{value}");
            let screen = modal_screen(&snapshot, &mut ui);
            let row = line_with(&screen, "log    ");
            assert!(row.contains(&format!("log                 {value}")), "{value}: {screen}");
            // Nothing steps it, so it never shows arrows.
            assert!(!row.contains('◂') && !row.contains('▸'), "{value}: {screen}");

            ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

            assert!(rx.try_recv().is_err(), "{value}: no SetSdLog");
        }
    }

    #[test]
    fn ui_asks_no_format_when_enter_pressed_on_sd_log_row() {
        // The row sits beside the format row, and a card that can be formatted is in the slot.
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdLog, &mut ui, &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(ui.modal().is_none(), "Enter applies the modal, as on any setting row");
        assert!(rx.try_recv().is_err(), "the row did not move, so nothing is sent");
        let rendered = modal_screen(&snapshot, &mut ui);
        assert!(!rendered.contains(" format sd card "), "no format confirm: {rendered}");
    }

    /// The settings modal drawn over the view, as text.
    fn modal_screen(snapshot: &Snapshot, ui: &mut Ui) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, ui)).expect("drawing");
        terminal.backend().to_string()
    }

    #[test]
    fn ui_sends_bluetooth_on_and_saves_none_of_it_when_row_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        select(Field::Bluetooth, &mut ui, &snapshot, &tx);
        assert!(modal_screen(&snapshot, &mut ui).contains("bluetooth         ◂ off ▸"));
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetBluetooth(true));
        assert!(rx.try_recv().is_err(), "nothing else was sent");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("bluetooth on"), "{notice}");
        let file = std::fs::read_to_string(&target).expect("the other rows were saved");
        assert!(!file.contains("bluetooth"), "the bridge's scan, not the file's: {file}");
    }

    #[test]
    fn ui_sends_bluetooth_off_when_row_turned_off_with_bluetooth_scanning() {
        let snapshot = Snapshot { scan: Some(ScanMode::WifiBluetooth), ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Bluetooth, &mut ui, &snapshot, &tx);
        let screen = modal_screen(&snapshot, &mut ui);
        assert!(screen.contains("bluetooth         ◂ on ▸"), "{screen}");
        ui.on_modal_key(key(KeyCode::Char('h')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetBluetooth(false));
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("bluetooth off"), "{notice}");
    }

    #[test]
    fn ui_sends_no_bluetooth_when_enter_pressed_with_row_unmoved() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Bluetooth, &mut ui, &snapshot, &tx);
        // Out and back: the scan the bridge already runs.
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Char('h')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "no SetBluetooth");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("bluetooth"), "{notice}");
    }

    #[test]
    fn ui_shows_scan_stopped_and_steps_nothing_when_scan_is_stopped() {
        // The run starts the scan once the bridge reports it stopped, so there is nothing to
        // toggle until then.
        let snapshot = Snapshot { scan: Some(ScanMode::Stopped), ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Bluetooth, &mut ui, &snapshot, &tx);
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").bluetooth, None);
        let screen = modal_screen(&snapshot, &mut ui);
        assert!(screen.contains("bluetooth           scan stopped"), "{screen}");

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "no SetBluetooth");
    }

    #[test]
    fn ui_sends_bluetooth_when_row_turned_on_before_bridge_reports_a_scan() {
        // The engine starts the scan on the bridge's first report, in the mode asked for
        // meanwhile.
        let snapshot = Snapshot { scan: None, ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::Bluetooth, &mut ui, &snapshot, &tx);
        assert_eq!(ui.modal().expect("open").bluetooth, Some(false));
        ui.on_modal_key(key(KeyCode::Char('l')), &snapshot, &tx);

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetBluetooth(true));
    }

    /// [`busy`] with the bridge reporting `sd`.
    fn sd_snapshot(sd: Option<SdStatus>) -> Snapshot {
        let mut snapshot = busy();
        if let Some(status) = snapshot.bridge_status.as_mut() {
            status.sd = sd;
        }
        snapshot
    }

    /// A "64 GB" card in `state`: 60,350 MiB, 58.9 GB.
    fn card(state: SdState, free_mb: Option<u32>) -> Option<SdStatus> {
        Some(SdStatus::card(state, 60_350, free_mb))
    }

    /// The first line of `screen` holding `text`.
    fn line_with<'a>(screen: &'a str, text: &str) -> &'a str {
        screen.lines().find(|line| line.contains(text)).unwrap_or_else(|| panic!("{screen}"))
    }

    #[test]
    fn ui_shows_sd_row_and_asks_only_where_card_can_be_formatted_when_enter_pressed() {
        let down = Snapshot { link_up: false, ..busy() };
        let no_card = Some(SdStatus::card(SdState::NoCard, 0, None));
        let cases = [
            (down, "no bridge", "no bridge to format a card on · esc cancel", false),
            (sd_snapshot(None), "no slot", "this bridge has no card slot · esc cancel", false),
            (sd_snapshot(no_card), "no card", "no card in the slot · esc cancel", false),
            (sd_snapshot(card(SdState::NoFat32, None)), "58.9 GB, not FAT32", FORMAT_TIP, true),
            (
                sd_snapshot(card(SdState::Ready, Some(55_000))),
                "53.7 of 58.9 GB free",
                FORMAT_TIP,
                true,
            ),
            (
                sd_snapshot(card(SdState::Formatting, None)),
                "formatting…",
                "a format is running · esc cancel",
                false,
            ),
            (sd_snapshot(card(SdState::Failed, None)), "card error", FORMAT_TIP, true),
        ];
        for (snapshot, value, tip, asks) in cases {
            let (tx, mut rx) = mpsc::channel(4);
            let mut ui = Ui::default();
            select(Field::SdCard, &mut ui, &snapshot, &tx);

            let screen = modal_screen(&snapshot, &mut ui);
            let row = line_with(&screen, "format   ");
            assert!(row.contains(&format!("format              {value}")), "{value}: {screen}");
            // Nothing steps it, so it never shows arrows.
            assert!(!row.contains('◂') && !row.contains('▸'), "{value}: {screen}");
            assert!(screen.contains(tip), "{value}: {screen}");

            ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);
            assert_eq!(ui.sd_confirming(), asks, "{value}");
            assert!(ui.modal().is_some(), "{value}: Enter on the row never closes the modal");
            assert!(rx.try_recv().is_err(), "{value}: nothing before the answer");
        }
    }

    #[test]
    fn ui_shows_sd_row_waiting_and_asks_nothing_when_bridge_has_sent_no_status() {
        // A link just up has no `Status` yet, so the slot is unknown rather than absent.
        let snapshot = Snapshot { bridge_status: None, ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdCard, &mut ui, &snapshot, &tx);

        let screen = modal_screen(&snapshot, &mut ui);
        let row = line_with(&screen, "format   ");
        assert!(row.contains("format              waiting"), "{screen}");
        assert!(screen.contains("waiting for the bridge's status · esc cancel"), "{screen}");

        ui.on_modal_key(key(KeyCode::Enter), &snapshot, &tx);
        assert!(!ui.sd_confirming());
        assert!(ui.modal().is_some(), "Enter on the row never closes the modal");
        assert!(rx.try_recv().is_err(), "nothing sent");
    }

    #[test]
    fn ui_leaves_sd_row_unchanged_when_h_or_l_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdCard, &mut ui, &snapshot, &tx);
        let before = modal_screen(&snapshot, &mut ui);
        for code in [KeyCode::Char('h'), KeyCode::Char('l'), KeyCode::Left, KeyCode::Right] {
            ui.on_modal_key(key(code), &snapshot, &tx);
        }
        assert_eq!(modal_screen(&snapshot, &mut ui), before);
        assert!(rx.try_recv().is_err());
    }

    /// The modal open on the sd card row of `snapshot`, its pool row moved to `eu`, and the
    /// format confirm open over it.
    fn sd_confirm_over_edits(snapshot: &Snapshot, tx: &mpsc::Sender<Command>) -> Ui {
        let mut ui = Ui::default();
        select(Field::Pool, &mut ui, snapshot, tx);
        ui.on_modal_key(key(KeyCode::Char('l')), snapshot, tx);
        while ui.modal().expect("open").selected != Field::SdCard {
            ui.on_modal_key(key(KeyCode::Down), snapshot, tx);
        }
        ui.on_modal_key(key(KeyCode::Enter), snapshot, tx);
        assert!(ui.sd_confirming());
        ui
    }

    #[test]
    fn ui_sends_format_sd_and_keeps_modal_edits_when_y_pressed_on_sd_confirm() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = sd_confirm_over_edits(&snapshot, &tx);
        let lines = ui.sd_confirm_lines().expect("the confirm");
        assert_eq!(
            lines,
            [
                "format the SD card as FAT32?",
                "everything on the 58.9 GB card is erased.",
                "",
                "y format · any other key cancels",
            ]
        );

        ui.on_sd_key(key(KeyCode::Char('y')), &snapshot, &tx);

        assert_eq!(rx.try_recv().expect("a command"), Command::FormatSd);
        assert!(rx.try_recv().is_err(), "the pool edit is not sent");
        assert!(!ui.sd_confirming());
        assert_eq!(ui.notice(snapshot.now_ms), Some("formatting the SD card…"));
        let modal = ui.modal().expect("still open");
        assert_eq!((modal.pool, modal.selected), (Some(ChannelPool::Eu), Field::SdCard));
    }

    #[test]
    fn ui_sends_nothing_and_keeps_modal_edits_when_other_key_pressed_on_sd_confirm() {
        let snapshot = busy();
        for code in [KeyCode::Char('n'), KeyCode::Char('Y'), KeyCode::Esc, KeyCode::Enter] {
            let (tx, mut rx) = mpsc::channel(4);
            let mut ui = sd_confirm_over_edits(&snapshot, &tx);

            ui.on_sd_key(key(code), &snapshot, &tx);

            assert!(rx.try_recv().is_err(), "{code:?}");
            assert!(!ui.sd_confirming(), "{code:?}");
            assert_eq!(ui.notice(snapshot.now_ms), Some("SD card not formatted"), "{code:?}");
            let modal = ui.modal().expect("still open");
            assert_eq!(modal.pool, Some(ChannelPool::Eu), "{code:?}");
        }
    }

    #[test]
    fn ui_sends_nothing_when_link_drops_before_sd_format_is_confirmed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = sd_confirm_over_edits(&snapshot, &tx);
        let down = Snapshot { link_up: false, ..snapshot };

        ui.on_sd_key(key(KeyCode::Char('y')), &down, &tx);

        assert!(rx.try_recv().is_err());
        assert!(!ui.sd_confirming());
        let notice = ui.notice(down.now_ms).expect("a reason");
        assert!(notice.contains("not connected"), "{notice}");
        assert_eq!(ui.modal().expect("still open").pool, Some(ChannelPool::Eu));
    }

    #[test]
    fn ui_says_engine_busy_when_sd_format_is_confirmed_with_queue_full() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(1);
        let mut ui = sd_confirm_over_edits(&snapshot, &tx);
        tx.try_send(Command::StopScan).expect("the one slot");

        ui.on_sd_key(key(KeyCode::Char('y')), &snapshot, &tx);

        assert_eq!(ui.notice(snapshot.now_ms), Some(ENGINE_BUSY));
        assert!(ui.modal().is_some());
    }

    #[test]
    fn ui_updates_sd_row_when_snapshot_changes_while_modal_open() {
        let blank = sd_snapshot(card(SdState::NoFat32, None));
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdCard, &mut ui, &blank, &tx);
        let screen = modal_screen(&blank, &mut ui);
        assert!(screen.contains("58.9 GB, not FAT32"), "{screen}");

        let formatting = sd_snapshot(card(SdState::Formatting, None));
        let screen = modal_screen(&formatting, &mut ui);
        assert!(screen.contains("format              formatting…"), "{screen}");
        assert!(screen.contains("a format is running · esc cancel"), "{screen}");

        let ready = sd_snapshot(card(SdState::Ready, Some(55_000)));
        let screen = modal_screen(&ready, &mut ui);
        assert!(screen.contains("format              53.7 of 58.9 GB free"), "{screen}");
        assert!(screen.contains(FORMAT_TIP), "{screen}");
    }

    /// The view at 58×26, the size of the Linux handheld, with the modal open on `field`.
    fn handheld(snapshot: &Snapshot, ui: &mut Ui) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(58, 26)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, ui)).expect("drawing");
        terminal
    }

    #[test]
    fn draw_fits_modal_with_sd_row_selected_when_terminal_is_58_by_26() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        select(Field::SdCard, &mut ui, &snapshot, &tx);
        let terminal = handheld(&snapshot, &mut ui);
        let rendered = terminal.backend().to_string();

        // 48×20, centered in 58×26: columns 5 to 52, rows 3 to 22.
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String { (5..53).map(|x| buffer[(x, y)].symbol()).collect() };
        assert!(row(3).starts_with("┌ settings "), "{rendered}");
        assert_eq!(row(22), format!("└{}┘", "─".repeat(46)), "{rendered}");
        assert_eq!(row(21), format!("│{FORMAT_TIP:<46}│"), "{rendered}");
        assert!(row(19).starts_with("│format              53.7 of 58.9 GB free"), "{rendered}");
        assert!(row(18).starts_with("│log               ◂ on ▸"), "{rendered}");
        assert!(row(17).starts_with("│sd card "), "{rendered}");
    }

    #[test]
    fn draw_renders_sd_confirm_over_settings_modal_when_enter_pressed_on_sd_row() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = sd_confirm_over_edits(&snapshot, &tx);
        let rendered = handheld(&snapshot, &mut ui).backend().to_string();

        assert!(rendered.contains(" format sd card "), "{rendered}");
        assert!(rendered.contains("everything on the 58.9 GB card is erased."), "{rendered}");
        assert!(rendered.contains("y format · any other key cancels"), "{rendered}");
        assert!(rendered.contains(" settings "), "the modal behind it: {rendered}");
    }
}
