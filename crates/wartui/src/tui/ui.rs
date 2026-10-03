use ratatui::crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;
use wartui_core::engine::{Command, Snapshot};
use wartui_proto::link::Mac;

use super::Settings;
use super::fleet::why_not_assignable;
use super::format::full_mac;
use super::settings::ConfigModal;
use super::upload::Upload;
use crate::config;

/// The notice when the engine takes no command: its queue is full, or it has stopped
/// and closed the channel.
pub(super) const ENGINE_BUSY: &str = "the engine is not accepting commands";

/// View state that outlives a frame: cursor, scroll offset, notice, settings modal,
/// saved settings and upload.
#[derive(Debug, Default)]
pub(super) struct Ui {
    /// The fleet table row under the cursor.
    pub(super) selected: usize,
    /// Scroll offset from the last frame. Recomputing from row 0 pins the cursor to
    /// the bottom edge, so `k` would scroll instead of move.
    pub(super) fleet_offset: usize,
    /// The notice, and the snapshot time it was sent.
    pub(super) notice: Option<(String, i64)>,
    /// The settings modal, while open.
    pub(super) modal: Option<ConfigModal>,
    /// Where to save, and what was last saved there.
    pub(super) settings: Settings,
    /// The upload `u` started.
    pub(super) upload: Upload,
}

/// How long a notice replaces the footer's key help and totals.
const NOTICE_MS: i64 = 4_000;

impl Ui {
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
            // Matched on the character: crossterm sends shift+r as `'R'`, not as
            // `'r'` with a modifier.
            KeyCode::Char('r') => self.clear_ring(snapshot, commands),
            KeyCode::Char('R') => self.clear_fleet_ring(snapshot, commands),
            KeyCode::Char('u') => {
                if let Some(text) = self.upload.press(&self.settings) {
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
        // Same rule as an assignment. The flag travels in the admin frame, and only a
        // heartbeat opens its window.
        if !holds && let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", full_mac(&target)), snapshot);
            return;
        }
        let assigned = if holds { None } else { Some(target) };
        let mut said = match commands.try_send(Command::AssignBle { mac: assigned }) {
            Ok(()) if holds => {
                format!("{}: bluetooth off on its next heartbeat", full_mac(&target))
            }
            Ok(()) => format!("{}: bluetooth on its next heartbeat", full_mac(&target)),
            Err(_) => {
                self.say(ENGINE_BUSY.to_owned(), snapshot);
                return;
            }
        };
        if snapshot.remember_ble {
            said.push_str(&self.remember_outcome(assigned));
        }
        self.say(said, snapshot);
    }

    /// Save the Bluetooth node `b` just chose to `wartui.toml`, and return the notice
    /// suffix in the style of [`Self::save_outcome`]. Nothing outside `[bluetooth]`
    /// changes.
    fn remember_outcome(&mut self, node: Option<Mac>) -> String {
        let Some(path) = self.settings.config_path.clone() else {
            return "; nowhere to save it — use --config".to_owned();
        };
        // Force `remember = true`. `b` saves only while the engine remembers, but
        // after a failed save `saved` may still say off, and `load` rejects off
        // beside a node.
        let mut written = self.settings.saved.clone();
        written.bluetooth = config::Bluetooth { remember: Some(true), node };
        match config::save(&path, &written) {
            Err(error) => format!("; could not save: {error}"),
            Ok(()) => {
                self.settings.saved = written;
                let done = if node.is_some() { "remembered" } else { "forgotten" };
                format!("; {done} in {}", path.display())
            }
        }
    }

    /// Clear the selected node's dedup ring on its next heartbeat.
    fn clear_ring(&mut self, snapshot: &Snapshot, commands: &mpsc::Sender<Command>) {
        let Some(node) = snapshot.nodes.get(self.selected) else { return };
        let target = node.state.mac;
        // Same refusal as `toggle_ble`. The frame travels in the admin window, and
        // only a heartbeat opens one.
        if let Some(why) = why_not_assignable(node) {
            self.say(format!("{} {why}", full_mac(&target)), snapshot);
            return;
        }
        let said = match commands.try_send(Command::ClearRing { mac: Some(target) }) {
            Ok(()) => {
                format!("{}: clearing its dedup ring on its next heartbeat", full_mac(&target))
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

    /// Take what the upload threads have reported since the last frame.
    pub(super) fn poll_upload(&mut self, snapshot: &Snapshot) {
        if let Some(text) = self.upload.poll() {
            self.say(text, snapshot);
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
    use wartui_proto::plan::{ChannelPool, Job, Radio, plan_for};

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
        snapshot.nodes.push(refused(0x23));
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
    fn ui_displays_next_heartbeat_notice_when_assigning_ble_to_surplus_node() {
        // A node the planner dealt nothing is still in the plan. Giving it the scan
        // deals it an empty share to carry the flag, which it hears on its next
        // heartbeat.
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
    fn ui_clamps_cursor_to_valid_row_when_navigating_or_fleet_size_changes() {
        let snapshot = busy();
        let (tx, _rx) = mpsc::channel(4);
        let mut ui = Ui::default();
        for _ in 0..10 {
            ui.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snapshot, &tx);
        }
        assert_eq!(ui.selected, snapshot.nodes.len() - 1);

        // A node ageing out of the table must not leave the cursor past its end.
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
        let mut ui = Ui {
            settings: Settings { config_path: Some(target.clone()), saved, ..Settings::default() },
            ..Ui::default()
        };

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
        let mut ui = Ui {
            settings: Settings { config_path: Some(target.clone()), ..Settings::default() },
            ..Ui::default()
        };

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
        let mut ui = Ui {
            settings: Settings { config_path: Some(target.clone()), saved, ..Settings::default() },
            ..Ui::default()
        };

        ui.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &snapshot, &tx);

        let file = config::load(Some(&target)).expect("a valid file");
        assert_eq!(file.bluetooth.node, Some(snapshot.nodes[0].state.mac), "b saved");
        assert_eq!(file.api_keys.wdgwars, "kept-key");
    }
}
