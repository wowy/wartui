use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use wartui_bridge::BridgeInfo;
use wartui_core::engine::{
    Assignment, BridgeStatus, Counters, NodeState, NodeView, Now, Snapshot, StoreStats, TailEntry,
};
use wartui_core::gps::{GpsCounters, GpsStatus, GpsView};
use wartui_core::position::{Fix, PositionSource};
use wartui_core::record::AdminOutcome;
use wartui_proto::air::{Capabilities, RecordKind, wire_epoch};
use wartui_proto::link::{Chip, LoopPhase, ResetCause};
use wartui_proto::plan::{ChannelPool, ChannelSet, DEFAULT_TX_POWER_QUARTER_DBM, IndexRun};

use super::draw;
use super::ui::Ui;

/// The fixtures' wall-clock start, in unix milliseconds.
pub(super) const EPOCH_MS: i64 = 1_777_642_477_000;

/// A heartbeating node with no assignment, ending in `last`.
pub(super) fn node(last: u8, reboots: u32, assignable: bool) -> NodeView {
    let now = Now { mono: Instant::now(), unix_ms: EPOCH_MS };
    let mut state = NodeState::new([0x02, 0x00, 0x5E, 0x10, 0x57, last], now);
    state.last_seen_ms = EPOCH_MS + 60_000;
    state.last_heartbeat = Some(now.mono);
    state.counter = Some(174);
    state.reboots = reboots;
    state.heartbeats = 12;
    state.observations = 340;
    state.link_rssi = Some(-41);
    // Heartbeating unless a test says otherwise. A node that has not heartbeated
    // cannot be assigned, so every assignment test would become a refusal test.
    state.capabilities = Some(Capabilities::here(true));
    NodeView { state, alive: true, assignable }
}

/// Heard only through sightings. Normal for a few seconds after boot: a node reports
/// what it found on a channel before it returns to the control channel to heartbeat.
pub(super) fn unannounced(last: u8) -> NodeView {
    let mut view = node(last, 0, false);
    view.state.capabilities = None;
    view.state.last_heartbeat = None;
    view.alive = false;
    view
}

/// A node whose token says 2.4 GHz only: an ESP32-C6.
pub(super) fn narrowband(last: u8) -> NodeView {
    let mut view = node(last, 0, true);
    view.state.capabilities = Some(Capabilities::here(false));
    view
}

/// Heartbeating stopped and nothing else is wrong. The only refusal whose cause is on
/// the node rather than this host.
pub(super) fn stale_node(last: u8) -> NodeView {
    let mut view = node(last, 0, false);
    // Heard, but not heartbeating: what `stale` means.
    view.alive = false;
    view
}

/// Heartbeating, but with no slot in the bridge's peer table.
pub(super) fn refused(last: u8) -> NodeView {
    let mut view = node(last, 0, false);
    view.state.peer_refused = true;
    view.state.last_outcome = Some(AdminOutcome::Refused);
    view
}

/// Set `held_epoch` to the epoch `desired` carries, or `confirmed` without one, as
/// the node's heartbeat would after adopting it. [`NodeState::adopted`] is then true.
pub(super) fn adopt(state: &mut NodeState) {
    let assignment = state.desired.or(state.confirmed).expect("something to adopt");
    state.held_epoch = Some(wire_epoch(assignment.counter));
}

/// Given channels, acked, and adopted: fully settled.
pub(super) fn assigned(last: u8) -> NodeView {
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

/// Acked its assignment, but its heartbeat does not yet say it holds it.
pub(super) fn acked_not_adopted(last: u8) -> NodeView {
    let mut view = assigned(last);
    view.state.held_epoch = None;
    view
}

/// Sent an assignment that its radio never acked. In practice BLE held the antenna
/// through the admin window.
pub(super) fn unacked(last: u8) -> NodeView {
    let mut view = pending(last);
    view.state.last_outcome = Some(AdminOutcome::Unacked);
    view.state.admin_attempts = 3;
    view
}

/// Given channels it has not had the chance to take. The window opens on its next
/// heartbeat.
pub(super) fn pending(last: u8) -> NodeView {
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

/// A running capture: five nodes in every assignment state, a full stream, and some
/// faults.
pub(super) fn busy() -> Snapshot {
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
        // Defaults, like `tx_power` and `bridge_tx_power` below, so an unrelated test
        // cannot trip the pool row's save rule.
        pool: ChannelPool::All,
        plan: None,
        ble_node: None,
        preferred_ble: None,
        remember_ble: true,
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

/// No bridge, no nodes, no position.
pub(super) fn empty() -> Snapshot {
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

/// [`busy`] with a receiver attached, in `status`.
pub(super) fn with_gps(
    status: GpsStatus,
    counters: GpsCounters,
    source: PositionSource,
) -> Snapshot {
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

/// [`with_gps`], for a receiver whose rate the operator set.
pub(super) fn with_pinned_gps(status: GpsStatus, counters: GpsCounters) -> Snapshot {
    let mut snapshot = with_gps(status, counters, PositionSource::Static);
    if let Some(gps) = snapshot.gps.as_mut() {
        gps.pinned_baud = true;
    }
    snapshot
}

/// `snapshot` drawn at 200×40, as text.
pub(super) fn rendered(snapshot: &Snapshot) -> String {
    let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
    terminal.draw(|frame| draw(frame, snapshot, &mut Ui::default())).expect("drawing");
    terminal.backend().to_string()
}
