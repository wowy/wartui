//! What the integration tests share: a store held in memory, and snapshots and the nodes in them.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use wartui_app::settings::{Config, SettingsStore, parse};
use wartui_bridge::BridgeInfo;
use wartui_core::engine::{
    BridgeStatus, EngineConfig, FleetEngine, NodeState, NodeView, Now, Snapshot, StoreStats,
};
use wartui_core::gps::{GpsCounters, GpsStatus, GpsView};
use wartui_core::nmea::Sky;
use wartui_core::position::DEFAULT_MAX_AGE;
use wartui_proto::bus::{Board, Capabilities, MAX_SLOTS, NodeBoard};
use wartui_proto::link::{BluetoothStatus, LoopPhase};
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

/// A snapshot of a running capture: the bridge announced, the link up, and `nodes`.
pub fn running(nodes: Vec<NodeView>) -> Snapshot {
    let mut snapshot = snapshot(Some(BRIDGE));
    snapshot.link_up = true;
    snapshot.bridge_status = Some(status());
    snapshot.alive = nodes.iter().filter(|node| node.alive).count();
    snapshot.assignable = nodes.iter().filter(|node| node.assignable).count();
    snapshot.nodes = nodes;
    snapshot
}

/// A bridge's report with nothing dropped, no fan and no Bluetooth host.
pub fn status() -> BridgeStatus {
    BridgeStatus {
        rx_count: 0,
        dropped_tx: 0,
        dropped_since_attach: 0,
        uptime_ms: 60_000,
        fan: None,
        sd: None,
        bluetooth: BluetoothStatus::default(),
        health: Default::default(),
    }
}

/// A heartbeating, assignable node ending in `last`, in the slot that byte names: `0x84` is A7,
/// `N7` on a 14-node carrier.
pub fn node(last: u8) -> NodeView {
    let now = Now { mono: Instant::now(), unix_ms: 1_777_642_477_000 };
    let mut state = NodeState::new([0x02, 0x00, 0x5E, 0x10, 0x57, last], now);
    state.last_heartbeat = Some(now.mono);
    state.observations = 340;
    state.slot = wartui_proto::bus::Slot::from_u8(last % MAX_SLOTS as u8);
    state.capabilities = Some(Capabilities::here(Board::Esp32c5));
    NodeView {
        state,
        alive: true,
        on_bus: true,
        assignable: true,
        share: None,
        adopted: false,
        starting: false,
    }
}

/// A host receiver in `status`, settled on a port, heard as the snapshot was taken.
pub fn receiver(status: GpsStatus, snapshot: &Snapshot) -> GpsView {
    GpsView {
        status,
        counters: GpsCounters::default(),
        last_fix_ms: None,
        settled: Some(("/dev/ttyACM1".to_owned(), 9_600)),
        pinned_baud: false,
        sky: Sky::default(),
        heard_ms: Some(snapshot.now_ms),
        max_age: DEFAULT_MAX_AGE,
        via_bridge: false,
    }
}
