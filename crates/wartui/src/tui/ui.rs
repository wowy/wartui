use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_scrollview::ScrollViewState;
use wartui_app::session::Controls;
use wartui_core::engine::{Command, NO_CARRIER, Snapshot};
use wartui_proto::bus::Slot;

use super::blocks::LOST_SLOT;
use super::gps;
use super::settings::{ConfigModal, ModalAction, apply, apply_excluded, unsaved};
use super::upload::{Upload, UploadTarget};
use crate::config;
use crate::text::{gb, node_name};

/// The notice when the engine takes no command: its queue is full, or it has stopped
/// and closed the channel.
pub(super) const ENGINE_BUSY: &str = "the engine is not accepting commands";

/// The notice when a power-cycle is asked for with the link down. The transport holds commands
/// while it reconnects, so one sent now would cut the node whenever the bridge comes back.
const NOT_CONNECTED: &str = "the bridge is not connected, so it cannot switch node power";

/// The notice when a confirmed format finds the link down, for the same reason: a format sent
/// now would erase the card whenever the bridge comes back.
const SD_NOT_CONNECTED: &str = "the bridge is not connected, so it cannot format its SD card";

/// View state that outlives a frame: cursor, scroll offset, notice, settings modal,
/// GPS modal, the capture's controls and upload.
#[derive(Debug)]
pub(super) struct Ui {
    /// The node block under the cursor.
    selected: usize,
    /// The block grid's scroll from the last frame. Recomputing from the top pins the cursor
    /// to the bottom edge, so `k` would scroll instead of move.
    grid: ScrollViewState,
    /// Blocks per row in the last frame, which `↑` and `↓` move by. 0 before the first.
    grid_cols: usize,
    /// The notice, and the snapshot time it was sent. Set by [`Self::say`], read by
    /// [`Self::notice`].
    notice: Option<(String, i64)>,
    /// The settings modal, while open.
    modal: Option<ConfigModal>,
    /// Whether the GPS modal is open.
    gps_open: bool,
    /// What steers the capture: its commands, and the settings it saves.
    controls: Controls,
    /// Where `u` uploads to.
    upload_target: UploadTarget,
    /// The upload `u` started.
    upload: Upload,
    /// The slot `p` asks to power-cycle, while its confirm is open.
    power_confirm: Option<Slot>,
    /// The last power-cycle answer shown, so each is shown once.
    power_shown: Option<u64>,
    /// The card's size in MiB, while the confirm to format it is open over the settings modal.
    sd_confirm: Option<u32>,
    /// The last format answer shown, so each is shown once.
    sd_shown: Option<u64>,
}

/// How long a notice replaces the footer's key help and totals.
const NOTICE_MS: i64 = 4_000;

impl Ui {
    /// A fresh view that steers the capture through `controls`.
    pub(super) fn new(controls: Controls) -> Self {
        Self {
            selected: 0,
            grid: ScrollViewState::default(),
            grid_cols: 0,
            notice: None,
            modal: None,
            gps_open: false,
            controls,
            upload_target: UploadTarget::default(),
            upload: Upload::default(),
            power_confirm: None,
            power_shown: None,
            sd_confirm: None,
            sd_shown: None,
        }
    }

    /// This view, with `u` uploading to `target`.
    pub(super) fn uploading_to(self, target: UploadTarget) -> Self {
        Self { upload_target: target, ..self }
    }

    /// The link this run reaches its bridge over. The simulator stands in for a bridge on USB, and
    /// reports USB as the bridge's host.
    pub(super) fn link(&self) -> config::Link {
        self.controls.settings().link().unwrap_or_default()
    }

    /// The block under the cursor, and the grid's scroll and blocks per row, for
    /// `draw_blocks` to read and update.
    pub(super) fn grid_mut(&mut self) -> (usize, &mut ScrollViewState, &mut usize) {
        (self.selected, &mut self.grid, &mut self.grid_cols)
    }

    /// The settings modal, while open.
    pub(super) fn modal(&self) -> Option<&ConfigModal> {
        self.modal.as_ref()
    }

    /// Whether the GPS modal is open.
    pub(super) fn gps_open(&self) -> bool {
        self.gps_open
    }

    /// Open the GPS modal.
    pub(super) fn open_gps(&mut self) {
        self.notice = None;
        self.show_gps(true);
    }

    /// A key while the GPS modal is open. It is read-only, so `esc`, `q` and `g` close it
    /// and every other key does nothing.
    pub(super) fn on_gps_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | 'g')) {
            self.show_gps(false);
        }
    }

    /// Open or close the GPS modal, which shows the receiver's sky. Every key that opens or closes
    /// it comes through here, so the session asks the bridge's receiver for its sky lists only
    /// while the modal shows them ([`Controls::show_sky`]).
    fn show_gps(&mut self, open: bool) {
        self.gps_open = open;
        self.controls.show_sky(open);
    }

    /// What steers the capture.
    #[cfg(test)]
    pub(super) fn controls(&self) -> &Controls {
        &self.controls
    }

    /// The upload `u` started.
    pub(super) fn upload(&self) -> &Upload {
        &self.upload
    }

    /// The upload `u` started, to set by hand.
    #[cfg(test)]
    pub(super) fn upload_mut(&mut self) -> &mut Upload {
        &mut self.upload
    }

    /// Keep the cursor on a real block as the fleet grows and shrinks.
    pub(super) fn clamp(&mut self, node_count: usize) {
        self.selected = self.selected.min(node_count.saturating_sub(1));
    }

    /// A key while no modal is open. No key here sends a command: the settings modal and the
    /// confirms do.
    pub(super) fn on_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        let last = snapshot.nodes.len().saturating_sub(1);
        let row = self.grid_cols.max(1);
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => self.select((self.selected + 1).min(last)),
            KeyCode::Left | KeyCode::Char('h') => self.select(self.selected.saturating_sub(1)),
            // A move off the grid's first or last row leaves the cursor where it is, rather
            // than sliding it sideways to the first or last block.
            KeyCode::Down | KeyCode::Char('j') => {
                let below = self.selected + row;
                self.select(if below <= last { below } else { self.selected });
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select(self.selected.checked_sub(row).unwrap_or(self.selected));
            }
            KeyCode::Char('c') => self.open_modal(snapshot),
            KeyCode::Char('g') => self.open_gps(),
            KeyCode::Char('p') => self.ask_power_cycle(snapshot),
            KeyCode::Char('u') => {
                let pressed = self.upload.press(
                    self.controls.settings(),
                    &self.upload_target,
                    &snapshot.excluded_ssids,
                );
                if let Some(text) = pressed {
                    self.say(text, snapshot);
                }
            }
            // Unbound keys leave the notice alone. It may be saying why the last key
            // did nothing.
            _ => {}
        }
    }

    /// Put the cursor on block `selected`, clearing the notice.
    fn select(&mut self, selected: usize) {
        self.notice = None;
        self.selected = selected;
    }

    /// Open the confirm to power-cycle the selected node, or say why not. A node needs only a
    /// slot: one that has stopped heartbeating is what a power-cycle is for.
    fn ask_power_cycle(&mut self, snapshot: &Snapshot) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        let name = node_name(snapshot.node_board(), node.state.slot, &target);
        if !snapshot.link_up {
            self.say(NOT_CONNECTED.to_owned(), snapshot);
            return;
        }
        // Refused here, so a bridge with no carrier is never sent a request it can only refuse.
        if !snapshot.node_board().is_carrier() {
            self.say(NO_CARRIER.to_owned(), snapshot);
            return;
        }
        let Some(slot) = node.state.slot else {
            let why = if node.state.capabilities.is_none() {
                "has not heartbeated yet, so nothing says which slot it is in"
            } else {
                LOST_SLOT
            };
            self.say(format!("{name} {why}"), snapshot);
            return;
        };
        self.notice = None;
        self.power_confirm = Some(slot);
    }

    /// Whether the power-cycle confirm is open, and so takes every key.
    pub(super) fn power_confirming(&self) -> bool {
        self.power_confirm.is_some()
    }

    /// A key while the power-cycle confirm is open: `y` sends, anything else cancels.
    pub(super) fn on_power_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        let Some(slot) = self.power_confirm.take() else { return };
        let name = snapshot.node_board().label(slot);
        if key.code != KeyCode::Char('y') || key.modifiers != KeyModifiers::NONE {
            self.say(format!("{name} not power-cycled"), snapshot);
            return;
        }
        // The link may have dropped while the confirm was open.
        if !snapshot.link_up {
            self.say(NOT_CONNECTED.to_owned(), snapshot);
            return;
        }
        let said = match self.controls.command(Command::PowerCycle { slot }) {
            Ok(()) => format!("power-cycling {name}"),
            Err(_) => ENGINE_BUSY.to_owned(),
        };
        self.say(said, snapshot);
    }

    /// The power-cycle confirm's lines, while it is open.
    pub(super) fn power_confirm_lines(&self, snapshot: &Snapshot) -> Option<Vec<String>> {
        let slot = self.power_confirm?;
        let name = snapshot.node_board().label(slot);
        Some(vec![
            format!("power-cycle {name}?"),
            "Its node goes off for a second, then boots again.".to_owned(),
            String::new(),
            "y power-cycle · any other key cancels".to_owned(),
        ])
    }

    /// A key while the run waits for a GPS fix ([`Snapshot::awaiting_fix`]). `view` routes
    /// every key but a quit here. `Space` ticks or unticks the box and saves it at once,
    /// `Enter` starts this run's scan without a fix, `g` opens the GPS modal over the pop-up,
    /// and every other key does nothing.
    pub(super) fn on_fix_wait_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        match key.code {
            KeyCode::Char(' ') => {
                let settings = self.controls.settings();
                let wait = !settings.saved().gps.waits_for_fix();
                // A failed save leaves the box as the file has it.
                if let Err(why) = settings.save_wait_for_fix(wait) {
                    let why = unsaved(&why);
                    self.say(format!("not saved: {}", why.trim_start_matches("; ")), snapshot);
                }
            }
            KeyCode::Enter => {
                let sent = self.controls.command(Command::StartWithoutFix);
                if sent.is_err() {
                    self.say(ENGINE_BUSY.to_owned(), snapshot);
                }
            }
            KeyCode::Char('g') => self.open_gps(),
            _ => {}
        }
    }

    /// The waiting-for-GPS pop-up's lines, while the run holds its scan for a fix. The box
    /// is what the file holds.
    pub(super) fn fix_wait_lines(&self, snapshot: &Snapshot) -> Option<Vec<String>> {
        if !snapshot.awaiting_fix {
            return None;
        }
        let tick = if self.controls.settings().waits_for_fix() { ' ' } else { 'x' };
        Some(vec![
            "Waiting for a GPS fix before the scan starts.".to_owned(),
            format!("gps: {}", gps::status(snapshot).0),
            String::new(),
            format!("[{tick}] skip this wait at every launch"),
            String::new(),
            "space tick · enter scan without a fix · g gps · q quit".to_owned(),
        ])
    }

    /// Open the confirm to format the bridge's SD card, over the settings modal.
    fn ask_format_sd(&mut self, snapshot: &Snapshot) {
        let Some(sd) = snapshot.bridge_status.and_then(|status| status.sd) else { return };
        self.notice = None;
        self.sd_confirm = Some(sd.card_mb);
    }

    /// Whether the format confirm is open, and so takes every key.
    pub(super) fn sd_confirming(&self) -> bool {
        self.sd_confirm.is_some()
    }

    /// A key while the format confirm is open: `y` sends, anything else cancels. The settings
    /// modal stays open either way, its edits untouched.
    pub(super) fn on_sd_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        if self.sd_confirm.take().is_none() {
            return;
        }
        if key.code != KeyCode::Char('y') || key.modifiers != KeyModifiers::NONE {
            self.say("SD card not formatted".to_owned(), snapshot);
            return;
        }
        // The link may have dropped while the confirm was open.
        if !snapshot.link_up {
            self.say(SD_NOT_CONNECTED.to_owned(), snapshot);
            return;
        }
        let said = match self.controls.command(Command::FormatSd) {
            Ok(()) => "formatting the SD card…".to_owned(),
            Err(_) => ENGINE_BUSY.to_owned(),
        };
        self.say(said, snapshot);
    }

    /// The format confirm's lines, while it is open.
    pub(super) fn sd_confirm_lines(&self) -> Option<Vec<String>> {
        let card_mb = self.sd_confirm?;
        Some(vec![
            "format the SD card as FAT32?".to_owned(),
            format!("everything on the {} GB card is erased.", gb(card_mb)),
            String::new(),
            "y format · any other key cancels".to_owned(),
        ])
    }

    /// Show the bridge's answer to a format, once.
    pub(super) fn poll_sd(&mut self, snapshot: &Snapshot) {
        let Some(answer) = &snapshot.sd_answer else { return };
        if self.sd_shown == Some(answer.id) {
            return;
        }
        self.sd_shown = Some(answer.id);
        self.say(answer.text.clone(), snapshot);
    }

    /// Show the bridge's answer to a power-cycle, once.
    pub(super) fn poll_power(&mut self, snapshot: &Snapshot) {
        let Some(answer) = &snapshot.power_answer else { return };
        if self.power_shown == Some(answer.id) {
            return;
        }
        self.power_shown = Some(answer.id);
        self.say(answer.text.clone(), snapshot);
    }

    /// Open the settings modal, seeded from what is in force.
    fn open_modal(&mut self, snapshot: &Snapshot) {
        self.notice = None;
        self.modal = Some(ConfigModal::open(snapshot, self.controls.settings()));
    }

    /// A key while the settings modal is open. `view` routes each key to this or to
    /// [`Self::on_key`], never both.
    pub(super) fn on_modal_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        let Some(modal) = self.modal.as_mut() else { return };
        match modal.on_key(key, snapshot) {
            // As in `on_key`, unbound keys leave the notice alone.
            ModalAction::Stay => {}
            ModalAction::Close => self.modal = None,
            ModalAction::Apply => match apply(modal, snapshot, &self.controls) {
                Ok(text) => {
                    self.modal = None;
                    self.say(text, snapshot);
                }
                // The modal stays open with its edits, for another `Enter`.
                Err(busy) => self.say(busy.to_owned(), snapshot),
            },
            ModalAction::FormatSd => self.ask_format_sd(snapshot),
            ModalAction::SaveExcluded => {
                match apply_excluded(modal, &self.controls) {
                    Ok(text) => {
                        modal.leave_page();
                        self.say(text, snapshot);
                    }
                    // The page stays open with its edits, for another `Enter`.
                    Err(busy) => self.say(busy.to_owned(), snapshot),
                }
            }
        }
    }

    /// A bracketed paste, which only the settings modal's text fields take.
    pub(super) fn on_modal_paste(&mut self, text: &str) {
        if let Some(modal) = self.modal.as_mut() {
            modal.paste(text);
        }
    }

    /// Take what the upload threads have reported since the last frame.
    pub(super) fn poll_upload(&mut self, snapshot: &Snapshot) {
        if let Some(text) = self.upload.poll() {
            self.say(text, snapshot);
        }
    }

    /// A key while the upload's confirm is open. Only `y` sends.
    pub(super) fn on_confirm_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        if let Some(text) = self.upload.on_confirm_key(key, &self.upload_target) {
            self.say(text, snapshot);
        }
    }

    /// Show `text` as the notice.
    pub(super) fn say(&mut self, text: String, snapshot: &Snapshot) {
        self.notice = Some((text, snapshot.now_ms));
    }

    /// The file as last loaded or saved.
    #[cfg(test)]
    pub(super) fn saved(&self) -> config::Config {
        self.controls.settings().saved()
    }

    /// The block grid's scroll offset, in rows.
    #[cfg(test)]
    pub(super) fn grid_offset(&self) -> u16 {
        self.grid.offset().y
    }

    /// The notice, while it is recent enough to be about what just happened: a key, or
    /// a report from the upload thread.
    pub(super) fn notice(&self, now_ms: i64) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| now_ms - at < NOTICE_MS)
            .map(|(text, _)| text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;
    use tokio::sync::mpsc;
    use wartui_app::settings::Settings;

    use super::*;
    use crate::tui::fixtures::*;

    fn press(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE)
    }

    #[test]
    fn ui_sends_power_cycle_when_p_then_y_is_pressed() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = on_board(NodeBoard::Carrier14);
        let slot = snapshot.nodes[0].state.slot.expect("a slot");
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_key(press('p'), &snapshot);
        assert!(ui.power_confirming());
        assert!(rx.try_recv().is_err(), "nothing before the answer");
        let lines = ui.power_confirm_lines(&snapshot).expect("the confirm");
        assert_eq!(lines[0], "power-cycle N7?", "{lines:?}");

        ui.on_power_key(press('y'), &snapshot);
        assert_eq!(rx.try_recv().expect("a command"), Command::PowerCycle { slot });
        assert!(!ui.power_confirming());
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert_eq!(notice, "power-cycling N7");
    }

    #[test]
    fn ui_sends_nothing_when_p_then_n_is_pressed() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = on_board(NodeBoard::Carrier14);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_key(press('p'), &snapshot);
        ui.on_power_key(press('n'), &snapshot);
        assert!(rx.try_recv().is_err());
        assert!(!ui.power_confirming());
        assert_eq!(ui.notice(snapshot.now_ms), Some("N7 not power-cycled"));
    }

    #[test]
    fn ui_refuses_power_cycle_without_asking_when_link_is_down() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = Snapshot { link_up: false, ..on_board(NodeBoard::Carrier14) };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_key(press('p'), &snapshot);
        assert!(!ui.power_confirming());
        assert!(rx.try_recv().is_err());
        assert_eq!(ui.notice(snapshot.now_ms), Some(NOT_CONNECTED));
    }

    #[test]
    fn ui_sends_nothing_when_link_drops_before_power_cycle_is_confirmed() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = on_board(NodeBoard::Carrier14);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_key(press('p'), &snapshot);
        assert!(ui.power_confirming());
        let down = Snapshot { link_up: false, ..snapshot };
        ui.on_power_key(press('y'), &down);
        assert!(rx.try_recv().is_err());
        assert!(!ui.power_confirming());
        assert_eq!(ui.notice(down.now_ms), Some(NOT_CONNECTED));
    }

    #[test]
    fn ui_refuses_power_cycle_without_asking_when_bridge_has_no_carrier() {
        let snapshot = on_board(wartui_proto::bus::NodeBoard::NoCarrier);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_key(press('p'), &snapshot);
        assert!(!ui.power_confirming());
        assert!(rx.try_recv().is_err());
        let notice = ui.notice(snapshot.now_ms).expect("a reason");
        assert!(notice.contains("no carrier board"), "{notice}");
    }

    #[test]
    fn ui_refuses_power_cycle_when_node_has_no_slot() {
        use wartui_proto::bus::NodeBoard;
        let (tx, mut rx) = mpsc::channel(4);
        for (row, want) in
            [(unslotted(0x23), "lost its bus slot"), (unannounced(0x24), "which slot")]
        {
            let mut row = row;
            row.state.slot = None;
            let snapshot = Snapshot { nodes: vec![row], ..on_board(NodeBoard::Carrier6) };
            let mut ui = steering(&tx, Settings::default());
            ui.on_key(press('p'), &snapshot);
            assert!(!ui.power_confirming(), "{want}");
            let notice = ui.notice(snapshot.now_ms).expect("a reason");
            assert!(notice.contains(want), "{notice}");
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ui_shows_power_cycle_answer_once_when_it_arrives() {
        use wartui_core::engine::PowerAnswer;
        let mut snapshot = busy();
        let mut ui = idle();
        let answer = PowerAnswer { id: 1, text: "N3 power-cycled".to_owned() };
        snapshot.power_answer = Some(answer);
        ui.poll_power(&snapshot);
        assert_eq!(ui.notice(snapshot.now_ms), Some("N3 power-cycled"));

        // The same answer in the next snapshot does not bring the notice back.
        ui.say("something since".to_owned(), &snapshot);
        ui.poll_power(&snapshot);
        assert_eq!(ui.notice(snapshot.now_ms), Some("something since"));
    }

    #[test]
    fn ui_shows_sd_format_answer_once_when_it_arrives() {
        use wartui_core::engine::SdAnswer;
        let mut snapshot = busy();
        let mut ui = idle();
        snapshot.sd_answer = Some(SdAnswer { id: 1, text: "SD card formatted".to_owned() });
        ui.poll_sd(&snapshot);
        assert_eq!(ui.notice(snapshot.now_ms), Some("SD card formatted"));

        // The same answer in the next snapshot does not bring the notice back.
        ui.say("something since".to_owned(), &snapshot);
        ui.poll_sd(&snapshot);
        assert_eq!(ui.notice(snapshot.now_ms), Some("something since"));

        // A new answer, even in the same words, is shown.
        snapshot.sd_answer = Some(SdAnswer { id: 2, text: "SD card formatted".to_owned() });
        ui.poll_sd(&snapshot);
        assert_eq!(ui.notice(snapshot.now_ms), Some("SD card formatted"));
    }

    #[test]
    fn ui_opens_gps_modal_when_g_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());
        ui.say("an old notice".to_owned(), &snapshot);
        ui.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &snapshot);
        assert!(ui.gps_open());
        assert!(rx.try_recv().is_err(), "nothing reaches the engine");
        assert_eq!(ui.notice(snapshot.now_ms), None);
    }

    #[test]
    fn ui_closes_gps_modal_when_esc_q_or_g_pressed() {
        for code in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('g')] {
            let mut ui = Ui { gps_open: true, ..idle() };
            ui.on_gps_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
            assert!(ui.gps_open(), "other keys leave it open");
            ui.on_gps_key(KeyEvent::new(code, KeyModifiers::NONE));
            assert!(!ui.gps_open(), "{code:?} closes it");
        }
    }

    #[test]
    fn ui_shows_sky_when_gps_modal_is_open() {
        let snapshot = with_bridge_sky(4);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());
        ui.controls().follow(&snapshot);
        assert!(rx.try_recv().is_err(), "closed from the start: the engine's default");

        ui.on_key(press('g'), &snapshot);
        assert!(rx.try_recv().is_err(), "the key itself sends nothing");
        ui.controls().follow(&snapshot);
        assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(true)));
        ui.controls().follow(&snapshot);
        assert!(rx.try_recv().is_err(), "once per change");

        ui.on_gps_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        ui.controls().follow(&snapshot);
        assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(false)));
        ui.controls().follow(&snapshot);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ui_clamps_cursor_to_valid_row_when_navigating_or_fleet_size_changes() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());
        for _ in 0..10 {
            ui.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snapshot);
        }
        assert_eq!(ui.selected, snapshot.nodes.len() - 1);

        // A node aging out of the table must not leave the cursor past its end.
        ui.clamp(1);
        assert_eq!(ui.selected, 0);
    }

    #[test]
    fn ui_moves_selection_by_row_when_down_is_pressed_on_grid() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui { grid_cols: 5, ..steering(&tx, Settings::default()) };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        ui.on_key(key(KeyCode::Down), &snapshot);
        assert_eq!(ui.selected, 5, "one row down");
        ui.on_key(key(KeyCode::Char('l')), &snapshot);
        assert_eq!(ui.selected, 6, "one block right");
        ui.on_key(key(KeyCode::Char('j')), &snapshot);
        assert_eq!(ui.selected, 11);
        ui.on_key(key(KeyCode::Up), &snapshot);
        assert_eq!(ui.selected, 6);
        ui.on_key(key(KeyCode::Char('h')), &snapshot);
        ui.on_key(key(KeyCode::Char('k')), &snapshot);
        assert_eq!(ui.selected, 0);
    }

    #[test]
    fn ui_keeps_selection_when_down_is_pressed_on_last_row() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut ui = Ui { grid_cols: 5, selected: 10, ..steering(&tx, Settings::default()) };
        ui.on_key(key, &snapshot);
        assert_eq!(ui.selected, 10, "N11 has no block below it");
        ui.selected = 4;
        ui.on_key(key, &snapshot);
        assert_eq!(ui.selected, 9);
        ui.on_key(key, &snapshot);
        assert_eq!(ui.selected, 9, "N10 has no block below it either");
    }

    #[test]
    fn ui_keeps_selection_when_up_is_pressed_on_first_row() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui { grid_cols: 5, selected: 3, ..steering(&tx, Settings::default()) };
        ui.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &snapshot);
        assert_eq!(ui.selected, 3);
    }

    #[test]
    fn ui_saves_skip_box_that_reloads_when_space_pressed_while_waiting_for_fix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, saving_to(&target));
        let lines = ui.fix_wait_lines(&snapshot).expect("the pop-up");
        assert!(lines.contains(&"[ ] skip this wait at every launch".to_owned()), "{lines:?}");
        assert!(lines.contains(&"gps: searching".to_owned()), "{lines:?}");

        ui.on_fix_wait_key(press(' '), &snapshot);

        assert!(rx.try_recv().is_err(), "the box sends nothing");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.gps.wait_for_fix, Some(false));
        let lines = ui.fix_wait_lines(&snapshot).expect("still up");
        assert!(lines.contains(&"[x] skip this wait at every launch".to_owned()), "{lines:?}");

        ui.on_fix_wait_key(press(' '), &snapshot);
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.gps.wait_for_fix, None, "unticked, the key drops out");
    }

    #[test]
    fn ui_keeps_box_and_says_why_when_space_pressed_with_nowhere_to_save() {
        let snapshot = awaiting_fix();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_fix_wait_key(press(' '), &snapshot);

        let lines = ui.fix_wait_lines(&snapshot).expect("the pop-up");
        assert!(lines.contains(&"[ ] skip this wait at every launch".to_owned()), "{lines:?}");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("--config"), "{notice}");
    }

    #[test]
    fn ui_sends_start_without_fix_when_enter_pressed_while_waiting_for_fix() {
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_fix_wait_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot);

        assert_eq!(rx.try_recv().unwrap(), Command::StartWithoutFix);
    }

    #[test]
    fn ui_sends_nothing_when_other_keys_pressed_while_waiting_for_fix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, saving_to(&target));

        for key in ['b', 'c', 'p', 'u', 'x', 'j'] {
            ui.on_fix_wait_key(press(key), &snapshot);
        }
        ui.on_fix_wait_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snapshot);

        assert!(rx.try_recv().is_err());
        assert!(ui.modal().is_none() && !ui.gps_open());
        assert!(!target.exists(), "nothing was written");
        assert_eq!(ui.notice(snapshot.now_ms), None);
    }

    #[test]
    fn ui_opens_gps_modal_when_g_pressed_while_waiting_for_fix() {
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_fix_wait_key(press('g'), &snapshot);

        assert!(ui.gps_open());
        assert!(rx.try_recv().is_err(), "nothing goes to the engine");
        ui.on_gps_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!ui.gps_open());
        assert!(ui.fix_wait_lines(&snapshot).is_some(), "the pop-up is there again");
    }

    #[test]
    fn ui_sends_gps_sky_when_gps_modal_opens_over_fix_wait() {
        let mut snapshot = awaiting_fix();
        snapshot.gps.as_mut().expect("a receiver").via_bridge = true;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_fix_wait_key(press('g'), &snapshot);
        ui.controls().follow(&snapshot);

        assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(true)));
    }

    #[test]
    fn ui_shows_no_fix_wait_lines_when_not_waiting() {
        assert_eq!(idle().fix_wait_lines(&busy()), None);
    }

    #[test]
    fn ui_sends_nothing_when_b_pressed() {
        // The Bluetooth scan is a settings row, so `b` is unbound.
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = steering(&tx, Settings::default());

        ui.on_key(press('b'), &snapshot);

        assert!(rx.try_recv().is_err());
        assert!(ui.modal().is_none());
        assert_eq!(ui.notice(snapshot.now_ms), None);
    }
}
