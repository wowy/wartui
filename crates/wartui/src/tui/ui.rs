use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;
use wartui_core::engine::{Command, Snapshot};
use wartui_proto::bus::Slot;
use wartui_proto::mac::{self, Mac};

use super::Settings;
use super::fleet::{LOST_SLOT, why_not_assignable};
use super::settings::{
    ConfigModal, ModalAction, apply, apply_excluded, save_ble_node, save_bridge,
};
use super::upload::Upload;
#[cfg(test)]
use crate::config;

/// The notice when the engine takes no command: its queue is full, or it has stopped
/// and closed the channel.
pub(super) const ENGINE_BUSY: &str = "the engine is not accepting commands";

/// The notice when `p` is pressed behind a bridge with no carrier board.
const NO_CARRIER: &str = "this bridge has no carrier board, so it cannot switch node power";

/// View state that outlives a frame: cursor, scroll offset, notice, settings modal,
/// GPS modal, saved settings and upload.
#[derive(Debug, Default)]
pub(super) struct Ui {
    /// The fleet table row under the cursor.
    selected: usize,
    /// Scroll offset from the last frame. Recomputing from row 0 pins the cursor to
    /// the bottom edge, so `k` would scroll instead of move.
    fleet_offset: usize,
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
    /// The node `p` asks to power-cycle, by slot and address, while its confirm is open.
    power_confirm: Option<(Slot, Mac)>,
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

    /// The fleet table row under the cursor.
    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    /// The fleet table's scroll offset, for `draw_fleet` to read and update.
    pub(super) fn fleet_offset_mut(&mut self) -> &mut usize {
        &mut self.fleet_offset
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

    /// Keep the cursor on a real row as the table grows and shrinks.
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
            KeyCode::Char('g') => self.open_gps(),
            KeyCode::Char('p') => self.ask_power_cycle(snapshot),
            // Matched on the character: crossterm sends shift+r as `'R'`, not as
            // `'r'` with a modifier.
            KeyCode::Char('r') => self.clear_ring(snapshot, commands),
            KeyCode::Char('R') => self.clear_fleet_ring(snapshot, commands),
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

    /// Move the Bluetooth scan onto the selected node, or off it.
    ///
    /// While the engine remembers the Bluetooth node, the choice is also saved to
    /// `wartui.toml`, so a restart gives the scan back to the same node.
    fn toggle_ble(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        let holds = snapshot.ble_node == Some(target);
        // Same rule as an assignment. The flag travels in the assignment, which answers
        // a heartbeat.
        if !holds && let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", mac::full(&target)), snapshot);
            return;
        }
        let assigned = if holds { None } else { Some(target) };
        let mut said = match commands.try_send(Command::AssignBle { mac: assigned }) {
            Ok(()) if holds => {
                format!("{}: bluetooth off on its next heartbeat", mac::full(&target))
            }
            Ok(()) => format!("{}: bluetooth on its next heartbeat", mac::full(&target)),
            Err(_) => {
                self.say(ENGINE_BUSY.to_owned(), snapshot);
                return;
            }
        };
        if snapshot.remember_ble {
            said.push_str(&save_ble_node(&mut self.settings, assigned));
        }
        self.say(said, snapshot);
    }

    /// Clear the selected node's dedup ring on its next heartbeat.
    fn clear_ring(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        // Same refusal as `toggle_ble`. The clear answers a heartbeat, and goes to the
        // slot that heartbeat came from.
        if let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", mac::full(&target)), snapshot);
            return;
        }
        let said = match commands.try_send(Command::ClearRing { mac: Some(target) }) {
            Ok(()) => {
                format!("{}: clearing its dedup ring on its next heartbeat", mac::full(&target))
            }
            Err(_) => ENGINE_BUSY.to_owned(),
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
            Err(_) => ENGINE_BUSY.to_owned(),
        };
        self.say(said, snapshot);
    }

    /// Open the confirm to power-cycle the selected node, or say why not. A node needs only a
    /// slot: one that has stopped heartbeating is what a power-cycle is for.
    fn ask_power_cycle(&mut self, snapshot: &Snapshot) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        // Refused here, so a bench bridge is never sent a request it can only refuse.
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
            self.say(format!("{} {why}", mac::full(&target)), snapshot);
            return;
        };
        self.notice = None;
        self.power_confirm = Some((slot, target));
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
        let Some((slot, target)) = self.power_confirm.take() else { return };
        let name = snapshot.node_board().label(slot);
        if key.code != KeyCode::Char('y') || key.modifiers != KeyModifiers::NONE {
            self.say(format!("{name} not power-cycled"), snapshot);
            return;
        }
        let said = match commands.try_send(Command::PowerCycle { slot }) {
            Ok(()) => format!("power-cycling {name} ({})", mac::full(&target)),
            Err(_) => ENGINE_BUSY.to_owned(),
        };
        self.say(said, snapshot);
    }

    /// The power-cycle confirm's lines, while it is open.
    pub(super) fn power_confirm_lines(&self, snapshot: &Snapshot) -> Option<Vec<String>> {
        let (slot, target) = self.power_confirm?;
        let name = snapshot.node_board().label(slot);
        Some(vec![
            format!("power-cycle {name} ({})?", mac::full(&target)),
            "Its node goes off for a second, then boots again.".to_owned(),
            String::new(),
            "y power-cycle · any other key cancels".to_owned(),
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

    /// The fleet table's scroll offset.
    #[cfg(test)]
    pub(super) fn fleet_offset(&self) -> usize {
        self.fleet_offset
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
    use crate::run::PoolArg;
    use crate::tui::fixtures::*;

    #[test]
    fn ui_displays_specific_refusal_notice_when_toggling_ble_on_unassignable_node() {
        // Three faults refuse `b`, and each needs a different fix, so each gets its
        // own wording.
        let mut snapshot = busy();
        snapshot.nodes.push(unannounced(0x21));
        snapshot.nodes.push(stale_node(0x22));
        snapshot.nodes.push(unslotted(0x23));
        snapshot.nodes.sort_by_key(|n| n.state.mac);
        // `expect`, not `continue`, so every reason is asserted.
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
            (row(0x23), "bus slot"),
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
    fn ui_displays_next_heartbeat_notice_when_assigning_ble_to_node_holding_nothing() {
        // A node with no share yet can still take the scan. It hears the flag on its next
        // heartbeat, with an empty share to carry it.
        let snapshot = busy();
        let idle = snapshot
            .nodes
            .iter()
            .position(|n| n.assignable && n.state.desired.is_none() && n.state.confirmed.is_none())
            .expect("a node with no share of its own");

        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui { selected: idle, ..Default::default() };
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert!(rx.try_recv().is_ok(), "the command goes out either way");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("on its next heartbeat"), "got {notice}");
    }

    #[test]
    fn ui_toggles_ble_assignment_when_b_key_is_pressed() {
        let snapshot = busy();
        let node = snapshot.nodes[0].state.mac;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::AssignBle { mac: Some(node) });

        // Pressed on the node holding the scan, it takes the scan off the fleet. This
        // is the only way back to no node scanning.
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
        assert!(lines[0].starts_with("power-cycle N7 (02:00:5E:10:57:84)?"), "{lines:?}");

        ui.on_power_key(press('y'), &snapshot, &tx);
        assert_eq!(rx.try_recv().expect("a command"), Command::PowerCycle { slot });
        assert!(!ui.power_confirming());
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.starts_with("power-cycling N7"), "{notice}");
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
    fn ui_refuses_power_cycle_without_asking_when_bridge_has_no_carrier() {
        let snapshot = busy();
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
    fn ui_sends_fleet_clear_ring_when_shift_r_key_is_pressed() {
        let snapshot = busy();
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        // crossterm sends shift+r as `'R'`, not `'r'` with a shift modifier.
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
    fn ui_saves_preferred_node_when_b_pressed_with_remember_on() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        // A pool already in the file. `b` saves around it, not over it.
        let saved = config::Config { pool: Some(PoolArg::Us), ..config::Config::default() };
        config::save(&target, &saved).unwrap();
        let snapshot = busy();
        let node = snapshot.nodes[0].state.mac;
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui =
            Ui::new(Settings { config_path: Some(target.clone()), saved, ..Settings::default() });

        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);

        assert_eq!(rx.try_recv().unwrap(), Command::AssignBle { mac: Some(node) });
        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.bluetooth.node, Some(node));
        assert_eq!(file.pool, Some(PoolArg::Us), "the pool already saved stays as it is");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(notice.contains("remembered in"), "{notice}");

        // Taking it off forgets it in the file too.
        let mut holding = busy();
        holding.ble_node = Some(node);
        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &holding, &tx);
        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.bluetooth.node, None);
        assert_eq!(file.pool, Some(PoolArg::Us));
        let notice = ui.notice(holding.now_ms).expect("a notice");
        assert!(notice.contains("forgotten in"), "{notice}");
    }

    #[test]
    fn ui_does_not_save_when_b_pressed_with_remember_off() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let snapshot = Snapshot { remember_ble: false, ..busy() };
        let (tx, mut rx) = mpsc::channel(4);
        let mut ui = Ui::new(Settings { config_path: Some(target.clone()), ..Settings::default() });

        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);

        assert!(rx.try_recv().is_ok(), "the scan still moves");
        assert!(!target.exists(), "nothing was written");
        let notice = ui.notice(snapshot.now_ms).expect("a notice");
        assert!(!notice.contains("save"), "{notice}");
    }

    #[test]
    fn ui_keeps_key_in_file_when_b_pressed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let saved = config::Config {
            api_keys: config::ApiKeys { wdgwars: "kept-key".to_owned() },
            ..config::Config::default()
        };
        config::save(&target, &saved).unwrap();
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui =
            Ui::new(Settings { config_path: Some(target.clone()), saved, ..Settings::default() });

        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);

        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.bluetooth.node, Some(snapshot.nodes[0].state.mac), "b saved");
        assert_eq!(file.api_keys.wdgwars, "kept-key");
    }
}
