//! What the integration tests share: a store held in memory, and snapshots.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use wartui_app::settings::{Config, SettingsStore, parse};
use wartui_bridge::BridgeInfo;
use wartui_core::engine::{EngineConfig, FleetEngine, Now, Snapshot, StoreStats};
use wartui_proto::bus::NodeBoard;
use wartui_proto::link::LoopPhase;
use wartui_proto::mac::Mac;
use wartui_proto::reset::ResetCause;

/// The bridge a snapshot has announced.
pub const BRIDGE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24];

/// A bridge other than the one connected.
pub const OTHER_BRIDGE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

/// A store held in memory, which counts its saves and fails them while `failing`.
#[derive(Debug, Default)]
pub struct Fake {
    pub text: Mutex<Option<String>>,
    saves: Mutex<usize>,
    failing: Mutex<bool>,
}

impl Fake {
    /// A store holding `text`.
    pub fn holding(text: &str) -> Arc<Self> {
        Arc::new(Self { text: Mutex::new(Some(text.to_owned())), ..Self::default() })
    }

    /// An empty store.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The settings it holds, which must parse.
    pub fn config(&self) -> Config {
        let text = self.text.lock().unwrap().clone().expect("something saved");
        parse(&text).expect("valid settings")
    }

    /// The text it holds.
    pub fn text(&self) -> Option<String> {
        self.text.lock().unwrap().clone()
    }

    /// How many saves it has taken.
    pub fn saves(&self) -> usize {
        *self.saves.lock().unwrap()
    }

    /// Fail every save from now on, or stop failing.
    pub fn fail(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }
}

impl SettingsStore for Fake {
    fn load(&self) -> std::io::Result<Option<String>> {
        Ok(self.text())
    }

    fn save(&self, text: &str, _secret: bool) -> std::io::Result<()> {
        if *self.failing.lock().unwrap() {
            return Err(std::io::Error::other("the store is full"));
        }
        *self.text.lock().unwrap() = Some(text.to_owned());
        *self.saves.lock().unwrap() += 1;
        Ok(())
    }

    fn place(&self) -> String {
        "wartui.toml".to_owned()
    }
}

/// A snapshot whose bridge, `bridge`, has announced itself, or none has.
pub fn snapshot(bridge: Option<Mac>) -> Snapshot {
    let now = Now { mono: Instant::now(), unix_ms: 1_777_642_477_000 };
    let engine = FleetEngine::new(EngineConfig::default(), now);
    let mut snapshot = engine.snapshot(now, StoreStats::default());
    snapshot.bridge = bridge.map(|mac| BridgeInfo {
        mac,
        fw_version: "0.1.0".to_owned(),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        uptime_ms: 1_000,
        node_board: NodeBoard::Carrier14,
        radio: None,
        image: None,
    });
    snapshot
}
