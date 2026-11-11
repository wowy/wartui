use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;
use tui_scrollview::ScrollViewState;
use wartui_core::engine::{Command, NO_CARRIER, Snapshot};
use wartui_proto::bus::Slot;
use wartui_proto::fleet::ScanMode;
use wartui_proto::mac::Mac;

use super::Settings;
use super::blocks::LOST_SLOT;
use super::gps;
use super::settings::{
    ConfigModal, ModalAction, apply, apply_excluded, save_bridge, save_fix_wait,
};
use super::upload::Upload;
#[cfg(test)]
use crate::config;
use crate::text::node_name;

/// The notice when the engine takes no command: its queue is full, or it has stopped
/// and closed the channel.
pub(super) const ENGINE_BUSY: &str = "the engine is not accepting commands";

/// The notice when `b` is pressed while the bridge's scan is stopped. The run starts the scan
/// whenever the bridge reports it stopped, and holds the port, so no other command can.
const SCAN_STOPPED: &str = "scan stopped: this run starts it once the bridge answers";

/// The notice when `b` adds Bluetooth to the scan. Both notices fit the 58-column screen.
const BLUETOOTH_ON: &str = "bluetooth on: the bridge picks a node once two can scan";

/// The notice when a power-cycle is asked for with the link down. The transport holds commands
/// while it reconnects, so one sent now would cut the node whenever the bridge comes back.
const NOT_CONNECTED: &str = "the bridge is not connected, so it cannot switch node power";

/// View state that outlives a frame: cursor, scroll offset, notice, settings modal,
/// GPS modal, saved settings and upload.
#[derive(Debug, Default)]
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
    /// Where to save, and what was last saved there.
    settings: Settings,
    /// The upload `u` started.
    upload: Upload,
    /// The bridge [`Self::poll_bridge`] last tried to save, so a failed save waits for
    /// the next change rather than repeating every frame.
    bridge_tried: Option<(Option<Mac>, Option<String>)>,
    /// The slot `p` asks to power-cycle, while its confirm is open.
    power_confirm: Option<Slot>,
    /// The last power-cycle answer shown, so each is shown once.
    power_shown: Option<u64>,
}

/// How long a notice replaces the footer's key help and totals.
const NOTICE_MS: i64 = 4_000;

impl Ui {
    /// A fresh view that saves through `settings`.
    pub(super) fn new(settings: Settings) -> Self {
        Self { settings, ..Self::default() }
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
        self.gps_open = true;
    }

    /// A key while the GPS modal is open. It is read-only, so `esc`, `q` and `g` close it
    /// and every other key does nothing.
    pub(super) fn on_gps_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | 'g')) {
            self.gps_open = false;
        }
    }

    /// The upload `u` started.
    pub(super) fn upload(&self) -> &Upload {
        &self.upload
    }

    /// Keep the cursor on a real block as the fleet grows and shrinks.
    pub(super) fn clamp(&mut self, node_count: usize) {
        self.selected = self.selected.min(node_count.saturating_sub(1));
    }

    /// A key while no modal is open.
    pub(super) fn on_key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        commands: &mpsc::Sender<Command>,
    ) {
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
            KeyCode::Char('b') => self.toggle_ble(snapshot, commands),
            KeyCode::Char('c') => self.open_modal(snapshot),
            KeyCode::Char('g') => self.open_gps(),
            KeyCode::Char('p') => self.ask_power_cycle(snapshot),
            KeyCode::Char('u') => {
                if let Some(text) = self.upload.press(&self.settings, &snapshot.excluded_ssids) {
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

    /// Ask the bridge to add the Bluetooth scan to the Wi-Fi scan, or take it off. The bridge
    /// picks the node, so the cursor plays no part.
    fn toggle_ble(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        if snapshot.scan == Some(ScanMode::Stopped) {
            self.say(SCAN_STOPPED.to_owned(), snapshot);
            return;
        }
        let on = snapshot.scan != Some(ScanMode::WifiBluetooth);
        let said = match commands.try_send(Command::SetBluetooth(on)) {
            Ok(()) if on => BLUETOOTH_ON.to_owned(),
            Ok(()) => "bluetooth off".to_owned(),
            Err(_) => ENGINE_BUSY.to_owned(),
        };
        self.say(said, snapshot);
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
    pub(super) fn on_power_key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        commands: &mpsc::Sender<Command>,
    ) {
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
        let said = match commands.try_send(Command::PowerCycle { slot }) {
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
    pub(super) fn on_fix_wait_key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        commands: &mpsc::Sender<Command>,
    ) {
        match key.code {
            KeyCode::Char(' ') => {
                let wait = !self.settings.saved.gps.waits_for_fix();
                // A failed save leaves the box as the file has it.
                if let Err(why) = save_fix_wait(&mut self.settings, wait) {
                    self.say(format!("not saved: {}", why.trim_start_matches("; ")), snapshot);
                }
            }
            KeyCode::Enter => {
                let sent = commands.try_send(Command::StartWithoutFix);
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
        let tick = if self.settings.saved.gps.waits_for_fix() { ' ' } else { 'x' };
        Some(vec![
            "Waiting for a GPS fix before the scan starts.".to_owned(),
            format!("gps: {}", gps::status(snapshot).0),
            String::new(),
            format!("[{tick}] skip this wait at every launch"),
            String::new(),
            "space tick · enter scan without a fix · g gps · q quit".to_owned(),
        ])
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
        self.modal = Some(ConfigModal::open(snapshot, &self.settings));
    }

    /// A key while the settings modal is open. `view` routes each key to this or to
    /// [`Self::on_key`], never both.
    pub(super) fn on_modal_key(
        &mut self,
        key: KeyEvent,
        snapshot: &Snapshot,
        commands: &mpsc::Sender<Command>,
    ) {
        let Some(modal) = self.modal.as_mut() else { return };
        match modal.on_key(key) {
            // As in `on_key`, unbound keys leave the notice alone.
            ModalAction::Stay => {}
            ModalAction::Close => self.modal = None,
            ModalAction::Apply => match apply(modal, snapshot, commands, &mut self.settings) {
                Ok(text) => {
                    self.modal = None;
                    self.say(text, snapshot);
                }
                // The modal stays open with its edits, for another `Enter`.
                Err(busy) => self.say(busy.to_owned(), snapshot),
            },
            ModalAction::SaveExcluded => {
                match apply_excluded(modal, commands, &mut self.settings) {
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

    /// Save the bridge the transport remembered or forgot since the last save.
    ///
    /// Silent, since nothing the operator did changed it: a failure is logged at `debug`
    /// and tried again only once the bridge changes.
    pub(super) fn poll_bridge(&mut self) {
        // Compared in place, since this runs every frame; cloned only once it has changed.
        let memory = &self.settings.bridge_memory;
        let saved = &self.settings.saved.bridge;
        if memory.matches(saved.mac, saved.serial.as_deref()) {
            // In step with the file, so a bridge whose save failed is tried again when it
            // comes back.
            self.bridge_tried = None;
            return;
        }
        if let Some((mac, serial)) = &self.bridge_tried
            && memory.matches(*mac, serial.as_deref())
        {
            return;
        }
        self.bridge_tried = Some((memory.recall(), memory.recall_serial()));
        if let Err(error) = save_bridge(&mut self.settings) {
            tracing::debug!(error = error.trim_start_matches("; "), "could not save the bridge");
        }
    }

    /// A key while the upload's confirm is open. Only `y` sends.
    pub(super) fn on_confirm_key(&mut self, key: KeyEvent, snapshot: &Snapshot) {
        if let Some(text) = self.upload.on_confirm_key(key, &self.settings.upload) {
            self.say(text, snapshot);
        }
    }

    /// Show `text` as the notice.
    pub(super) fn say(&mut self, text: String, snapshot: &Snapshot) {
        self.notice = Some((text, snapshot.now_ms));
    }

    /// The file as last loaded or saved.
    #[cfg(test)]
    pub(super) fn saved(&self) -> &config::Config {
        &self.settings.saved
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

    use super::*;
    use crate::tui::fixtures::*;

    #[test]
    fn ui_toggles_bluetooth_scan_when_b_key_is_pressed() {
        let mut snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::SetBluetooth(true));
        assert!(ui.notice(snapshot.now_ms).expect("a notice").starts_with("bluetooth on"));

        // Whichever node is selected: the bridge picks the one that scans.
        snapshot.scan = Some(ScanMode::WifiBluetooth);
        let mut ui = Ui { selected: 3, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::SetBluetooth(false));
        assert!(ui.notice(snapshot.now_ms).expect("a notice").contains("bluetooth off"));
    }

    #[test]
    fn ui_says_how_scan_starts_when_b_is_pressed_while_scan_is_stopped() {
        let snapshot = Snapshot { scan: Some(ScanMode::Stopped), ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert!(rx.try_recv().is_err(), "nothing to toggle");
        assert_eq!(ui.notice(snapshot.now_ms), Some(SCAN_STOPPED));
        // `wartui scan start` cannot reach a bridge this run holds, so the notice does not offer it.
        assert!(!SCAN_STOPPED.contains("wartui scan"), "{SCAN_STOPPED}");
    }

    #[test]
    fn ui_fits_bluetooth_notices_on_one_line_when_screen_is_58_columns() {
        // The footer pads a notice with one space, and clips what passes the edge.
        for notice in [SCAN_STOPPED, BLUETOOTH_ON] {
            assert!(notice.chars().count() < 58, "{notice}");
        }
    }

    #[test]
    fn view_shows_scan_stopped_notice_in_footer_when_b_is_pressed_while_scan_is_stopped() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let snapshot = Snapshot { scan: Some(ScanMode::Stopped), ..busy() };
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| crate::tui::draw(frame, &snapshot, &mut ui)).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains(SCAN_STOPPED), "{screen}");
        assert!(!screen.contains("q quit"), "the notice replaces the key help: {screen}");
    }

    #[test]
    fn ui_asks_for_bluetooth_when_b_is_pressed_before_bridge_reports_a_scan() {
        // The run starts the scan on the bridge's first status, in the mode asked for meanwhile.
        let snapshot = Snapshot { scan: None, ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::SetBluetooth(true));
        assert_eq!(ui.notice(snapshot.now_ms), Some(BLUETOOTH_ON));
    }

    fn press(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE)
    }

    #[test]
    fn ui_sends_power_cycle_when_p_then_y_is_pressed() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = on_board(NodeBoard::Carrier14);
        let slot = snapshot.nodes[0].state.slot.expect("a slot");
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(press('p'), &snapshot, &tx);
        assert!(ui.power_confirming());
        assert!(rx.try_recv().is_err(), "nothing before the answer");
        let lines = ui.power_confirm_lines(&snapshot).expect("the confirm");
        assert_eq!(lines[0], "power-cycle N7?", "{lines:?}");

        ui.on_power_key(press('y'), &snapshot, &tx);
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
        let mut ui = Ui::default();

        ui.on_key(press('p'), &snapshot, &tx);
        ui.on_power_key(press('n'), &snapshot, &tx);
        assert!(rx.try_recv().is_err());
        assert!(!ui.power_confirming());
        assert_eq!(ui.notice(snapshot.now_ms), Some("N7 not power-cycled"));
    }

    #[test]
    fn ui_refuses_power_cycle_without_asking_when_link_is_down() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = Snapshot { link_up: false, ..on_board(NodeBoard::Carrier14) };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(press('p'), &snapshot, &tx);
        assert!(!ui.power_confirming());
        assert!(rx.try_recv().is_err());
        assert_eq!(ui.notice(snapshot.now_ms), Some(NOT_CONNECTED));
    }

    #[test]
    fn ui_sends_nothing_when_link_drops_before_power_cycle_is_confirmed() {
        use wartui_proto::bus::NodeBoard;
        let snapshot = on_board(NodeBoard::Carrier14);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(press('p'), &snapshot, &tx);
        assert!(ui.power_confirming());
        let down = Snapshot { link_up: false, ..snapshot };
        ui.on_power_key(press('y'), &down, &tx);
        assert!(rx.try_recv().is_err());
        assert!(!ui.power_confirming());
        assert_eq!(ui.notice(down.now_ms), Some(NOT_CONNECTED));
    }

    #[test]
    fn ui_refuses_power_cycle_without_asking_when_bridge_has_no_carrier() {
        let snapshot = on_board(wartui_proto::bus::NodeBoard::NoCarrier);
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_key(press('p'), &snapshot, &tx);
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
            let mut ui = Ui::default();
            ui.on_key(press('p'), &snapshot, &tx);
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
        let mut ui = Ui::default();
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
    fn ui_opens_gps_modal_when_g_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.say("an old notice".to_owned(), &snapshot);
        ui.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &snapshot, &tx);
        assert!(ui.gps_open());
        assert!(rx.try_recv().is_err(), "nothing reaches the engine");
        assert_eq!(ui.notice(snapshot.now_ms), None);
    }

    #[test]
    fn ui_closes_gps_modal_when_esc_q_or_g_pressed() {
        for code in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('g')] {
            let mut ui = Ui { gps_open: true, ..Default::default() };
            ui.on_gps_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
            assert!(ui.gps_open(), "other keys leave it open");
            ui.on_gps_key(KeyEvent::new(code, KeyModifiers::NONE));
            assert!(!ui.gps_open(), "{code:?} closes it");
        }
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

        // A node aging out of the table must not leave the cursor past its end.
        ui.clamp(1);
        assert_eq!(ui.selected, 0);
    }

    #[test]
    fn ui_moves_selection_by_row_when_down_is_pressed_on_grid() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui { grid_cols: 5, ..Default::default() };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        ui.on_key(key(KeyCode::Down), &snapshot, &tx);
        assert_eq!(ui.selected, 5, "one row down");
        ui.on_key(key(KeyCode::Char('l')), &snapshot, &tx);
        assert_eq!(ui.selected, 6, "one block right");
        ui.on_key(key(KeyCode::Char('j')), &snapshot, &tx);
        assert_eq!(ui.selected, 11);
        ui.on_key(key(KeyCode::Up), &snapshot, &tx);
        assert_eq!(ui.selected, 6);
        ui.on_key(key(KeyCode::Char('h')), &snapshot, &tx);
        ui.on_key(key(KeyCode::Char('k')), &snapshot, &tx);
        assert_eq!(ui.selected, 0);
    }

    #[test]
    fn ui_keeps_selection_when_down_is_pressed_on_last_row() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut ui = Ui { grid_cols: 5, selected: 10, ..Default::default() };
        ui.on_key(key, &snapshot, &tx);
        assert_eq!(ui.selected, 10, "N11 has no block below it");
        ui.selected = 4;
        ui.on_key(key, &snapshot, &tx);
        assert_eq!(ui.selected, 9);
        ui.on_key(key, &snapshot, &tx);
        assert_eq!(ui.selected, 9, "N10 has no block below it either");
    }

    #[test]
    fn ui_keeps_selection_when_up_is_pressed_on_first_row() {
        let snapshot = fleet_of(14);
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui { grid_cols: 5, selected: 3, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(ui.selected, 3);
    }

    #[test]
    fn ui_saves_skip_box_that_reloads_when_space_pressed_while_waiting_for_fix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });
        let lines = ui.fix_wait_lines(&snapshot).expect("the pop-up");
        assert!(lines.contains(&"[ ] skip this wait at every launch".to_owned()), "{lines:?}");
        assert!(lines.contains(&"gps: searching".to_owned()), "{lines:?}");

        ui.on_fix_wait_key(press(' '), &snapshot, &tx);

        assert!(rx.try_recv().is_err(), "the box sends nothing");
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.gps.wait_for_fix, Some(false));
        let lines = ui.fix_wait_lines(&snapshot).expect("still up");
        assert!(lines.contains(&"[x] skip this wait at every launch".to_owned()), "{lines:?}");

        ui.on_fix_wait_key(press(' '), &snapshot, &tx);
        let saved = config::load(Some(&target)).expect("a valid file");
        assert_eq!(saved.gps.wait_for_fix, None, "unticked, the key drops out");
    }

    #[test]
    fn ui_keeps_box_and_says_why_when_space_pressed_with_nowhere_to_save() {
        let snapshot = awaiting_fix();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_fix_wait_key(press(' '), &snapshot, &tx);

        let lines = ui.fix_wait_lines(&snapshot).expect("the pop-up");
        assert!(lines.contains(&"[ ] skip this wait at every launch".to_owned()), "{lines:?}");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("--config"), "{notice}");
    }

    #[test]
    fn ui_sends_start_without_fix_when_enter_pressed_while_waiting_for_fix() {
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_fix_wait_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::StartWithoutFix);
    }

    #[test]
    fn ui_sends_nothing_when_other_keys_pressed_while_waiting_for_fix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });

        for key in ['b', 'c', 'p', 'u', 'x', 'j'] {
            ui.on_fix_wait_key(press(key), &snapshot, &tx);
        }
        ui.on_fix_wait_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_err());
        assert!(ui.modal().is_none() && !ui.gps_open());
        assert!(!target.exists(), "nothing was written");
        assert_eq!(ui.notice(snapshot.now_ms), None);
    }

    #[test]
    fn ui_opens_gps_modal_when_g_pressed_while_waiting_for_fix() {
        let snapshot = awaiting_fix();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();

        ui.on_fix_wait_key(press('g'), &snapshot, &tx);

        assert!(ui.gps_open());
        assert!(rx.try_recv().is_err(), "nothing goes to the engine");
        ui.on_gps_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!ui.gps_open());
        assert!(ui.fix_wait_lines(&snapshot).is_some(), "the pop-up is there again");
    }

    #[test]
    fn ui_shows_no_fix_wait_lines_when_not_waiting() {
        assert_eq!(Ui::default().fix_wait_lines(&busy()), None);
    }

    #[test]
    fn ui_writes_nothing_to_config_when_b_pressed() {
        // The bridge picks the Bluetooth node, so `b` leaves `wartui.toml` alone.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });

        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::SetBluetooth(true));
        assert!(!target.exists(), "nothing was written");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert_eq!(notice, BLUETOOTH_ON);
    }
}
