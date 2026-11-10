//! The whole chain, with a simulated fleet standing in for hardware.
//!
//! Every piece below is covered on its own elsewhere. This is the test that the
//! pieces are actually connected: that a frame arriving on the link becomes a
//! row in SQLite becomes a line in a file WiGLE would accept, without anyone
//! having to plug anything in.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::OptionalExtension;
use tokio::sync::{oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{Command, EngineConfig, FleetEngine, StoreStats};
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::gps::Gps;
use wartui_core::position::PositionChain;
use wartui_core::runtime::{drive, now};
use wartui_core::store::{CaptureInfo, Store, StoreConfig, open_readonly};

#[tokio::test(start_paused = true)]
async fn drive_stores_sightings_that_export_when_fleet_simulated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // Fast-forwarded, so the two seconds below cover whole sweeps.
    let link = SimTransport::new(SimConfig { node_count: 3, speed: 60.0, ..Default::default() })
        .start()
        .expect("starting the simulator");

    let started = now();
    let info = CaptureInfo::default();
    let store = Store::create(&StoreConfig::new(&path), &info, started.unix_ms)
        .expect("creating the store");

    let config = EngineConfig {
        auto_start: true,
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0)),
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);
    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();

    let (_command_tx, command_rx) = tokio::sync::mpsc::channel(4);
    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    tokio::time::sleep(Duration::from_secs(2)).await;
    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    // The snapshot the UI would have been drawing.
    let snapshot = snapshot_rx.borrow().clone();
    assert_eq!(snapshot.nodes.len(), 3, "every simulated node should have been seen");
    assert!(snapshot.counters.observations > 0, "and should have reported something");
    assert_eq!(snapshot.counters.undecodable, 0, "the decoder agrees with the simulator");
    assert_eq!(snapshot.counters.unknown_slot, 0, "every node heartbeats before it reports");
    assert_eq!(snapshot.store.dropped, 0, "nothing should be dropped at this rate");

    let conn = open_readonly(&path).expect("reopening the capture");
    let stored: i64 =
        conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).expect("counting");
    assert!(stored > 0, "observations should have reached the disk");

    let (ended, bridge): (Option<i64>, Option<Vec<u8>>) = conn
        .query_row("SELECT ended_at, bridge_mac FROM capture", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("capture row");
    assert!(ended.is_some(), "a stopped capture should close its capture row out");
    assert!(bridge.is_some(), "and should say which bridge it came through");

    // The P4's plan as its `Fleet` reports gave it: a share per slot, which a later heartbeat
    // from that slot reports holding, and that heartbeat names the node.
    let shares: Vec<(String, i64, u8)> = conn
        .prepare("SELECT slot, rx_at, epoch FROM assignment WHERE epoch != 0 ORDER BY id")
        .expect("shares")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("share rows")
        .collect::<Result<_, _>>()
        .expect("share values");
    let mut nodes = std::collections::HashSet::new();
    for (slot, rx_at, epoch) in &shares {
        let node: Option<Vec<u8>> = conn
            .query_row(
                "SELECT node_mac FROM heartbeat
                 WHERE slot = ?1 AND rx_at >= ?2 AND epoch = ?3 ORDER BY id LIMIT 1",
                rusqlite::params![slot, rx_at, epoch],
                |r| r.get(0),
            )
            .optional()
            .expect("adoption");
        let node = node.unwrap_or_else(|| panic!("a heartbeat from {slot} reports epoch {epoch}"));
        nodes.insert(node);
    }
    assert_eq!(nodes.len(), 3, "a share row per node: {shares:?}");
    // The clean stop withdraws every share.
    let last: Vec<u8> = conn
        .prepare(
            "SELECT epoch FROM assignment
             WHERE id IN (SELECT MAX(id) FROM assignment GROUP BY slot)",
        )
        .expect("last rows")
        .query_map([], |r| r.get(0))
        .expect("last row per slot")
        .collect::<Result<_, _>>()
        .expect("epochs");
    assert_eq!(last, vec![0, 0, 0], "each slot ends withdrawn");

    // The P4 boots stopped, the run starts it, and the clean quit stops it. The runtime records
    // changes, not every event or tick.
    let modes: Vec<String> = conn
        .prepare("SELECT v FROM kv WHERE k LIKE 'capture.settings.%' ORDER BY k")
        .expect("settings history")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("settings rows")
        .map(|v| {
            let v = v.expect("settings text");
            v.split(';').find_map(|kv| kv.strip_prefix("mode=")).expect("a mode").to_owned()
        })
        .collect();
    let mut changes = modes.clone();
    changes.dedup();
    assert_eq!(changes, ["none", "stopped", "wifi", "stopped"], "{modes:?}");

    let mut csv = Vec::new();
    let summary = wigle_csv(&conn, &ExportFilter::default(), &mut csv, "0.1.0").expect("exporting");
    let csv = String::from_utf8(csv).expect("the CSV is UTF-8");

    assert!(summary.rows > 0);
    assert_eq!(summary.unpositioned, 0, "a static position covers every row");
    assert_eq!(
        csv.lines().count() as u64,
        summary.rows + 2,
        "two header lines and one row per recapture window"
    );
    for row in csv.lines().skip(2) {
        let fields: Vec<&str> = row.split(',').collect();
        assert_eq!(fields.len(), 14, "WiGLE v1.6 has fourteen columns: {row}");
        assert!(matches!(fields[13], "WIFI" | "BLE"), "{row}");
        assert_eq!(fields[7], "37.7749", "{row}");
    }
    assert!(
        csv.contains("5A03BA0000 BAA2D00000 BAA2D02000"),
        "the simulated Passpoint networks carry their roaming consortium: {csv}"
    );
}

#[tokio::test(start_paused = true)]
async fn drive_records_settings_history_when_operator_changes_pool_and_bluetooth() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.db");
    let link = SimTransport::new(SimConfig { node_count: 2, speed: 60.0, ..Default::default() })
        .start()
        .expect("simulator");
    let started = now();
    let store = Store::create(&StoreConfig::new(&path), &CaptureInfo::default(), started.unix_ms)
        .expect("store");
    let config = EngineConfig { auto_start: true, ..EngineConfig::default() };
    let engine = FleetEngine::new(config, started);
    let (snapshot_tx, mut snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (command_tx, command_rx) = tokio::sync::mpsc::channel(4);
    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    // The bridge's first `Status`, so the change below is told apart from it, and both nodes,
    // so the bridge has two to give Bluetooth to one of.
    snapshot_rx
        .wait_for(|s| s.pool.is_some() && s.nodes.iter().filter(|n| n.adopted).count() == 2)
        .await
        .expect("the bridge reports a pool and the scan starts");
    command_tx
        .send(Command::SetPool { pool: wartui_proto::plan::ChannelPool::Us })
        .await
        .expect("pool command");
    command_tx
        .send(Command::SetPool { pool: wartui_proto::plan::ChannelPool::Us })
        .await
        .expect("same pool");
    // The bridge's answer, so the pool and the Bluetooth slot change in separate rows.
    snapshot_rx
        .wait_for(|s| s.pool == Some(wartui_proto::plan::ChannelPool::Us))
        .await
        .expect("the bridge takes the pool");
    command_tx.send(Command::SetBluetooth(true)).await.expect("bluetooth command");
    snapshot_rx
        .wait_for(|s| s.ble_slot.is_some())
        .await
        .expect("the bridge gives one node the scan");
    tokio::time::sleep(Duration::from_secs(1)).await;
    stop_tx.send(()).expect("stop");
    assert_eq!(capture.await.expect("capture").dropped, 0);
    let conn = open_readonly(&path).expect("read-only capture");
    let mut stmt = conn
        .prepare("SELECT v FROM kv WHERE k LIKE 'capture.settings.%' ORDER BY k")
        .expect("history");
    let values = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("settings strings");
    // The scan mode changes in rows of its own as the run starts and stops the P4, so set it and
    // the time aside to read the pool and Bluetooth history.
    let mut history: Vec<String> = values
        .iter()
        .map(|v| {
            v.split(';')
                .filter(|kv| !kv.starts_with("at_ms=") && !kv.starts_with("mode="))
                .collect::<Vec<_>>()
                .join(";")
        })
        .collect();
    history.dedup();
    assert_eq!(history.len(), 4, "initial settings and three changes, not the repeated request");
    assert!(history[0].contains("pool=none;"), "before the bridge's first `Status`");
    assert!(history[1].contains("pool=all;"));
    assert!(history[2].contains("pool=us;") && history[2].contains("ble_slot=none;"));
    assert!(values.iter().any(|v| v.contains(";mode=wifi+bluetooth;")), "{values:?}");
    assert!(values.last().expect("a row").contains(";mode=stopped;"), "a clean quit stops");
    // The highest slot, which the bridge's planner picks.
    let node = wartui_proto::mac::full(&SimTransport::node_mac(1)).to_string();
    assert!(
        history[3].contains("ble_slot=A2;") && history[3].ends_with(&format!("ble_node={node}"))
    );
}

/// Two fixes a few streets apart, so a row can be told which one it was written
/// under. Both are somewhere in Munich; the static fallback below is in San
/// Francisco, which makes the tier that answered obvious from the coordinates.
const FIRST: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
const SECOND: &[u8] = b"$GPGGA,123529.00,4810.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*6C";

#[tokio::test(start_paused = true)]
async fn drive_attaches_gps_positions_when_receiver_moves() {
    // The point of the GPS tier: not one position for the run, but the position
    // at the moment each observation arrived.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("drive.db");

    let link = SimTransport::new(SimConfig { node_count: 2, speed: 60.0, ..Default::default() })
        .start()
        .expect("starting the simulator");

    let started = now();
    let info = CaptureInfo::default();
    let store = Store::create(&StoreConfig::new(&path), &info, started.unix_ms)
        .expect("creating the store");

    let gps = Gps::detached();
    gps.feed(FIRST, started.unix_ms);
    let config = EngineConfig {
        auto_start: true,
        // A static position underneath, to prove the receiver is what the rows
        // actually used rather than the only thing they could have used.
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0))
            .with_max_age(Duration::from_secs(60))
            .with_gps(gps.clone()),
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);
    let (snapshot_tx, _snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (command_tx, command_rx) = tokio::sync::mpsc::channel(4);

    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    // One node scans Bluetooth, which is what keeps observations arriving for the
    // whole drive: advertisers rotate their addresses and never fall into the dedup
    // ring, while the access points are all reported in the first sweep. Without a
    // live stream there is nothing for the second fix to be attached to.
    command_tx.send(Command::SetBluetooth(true)).await.expect("the engine is listening");
    tokio::time::sleep(Duration::from_millis(700)).await;
    gps.feed(SECOND, now().unix_ms);
    tokio::time::sleep(Duration::from_millis(700)).await;
    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    let conn = open_readonly(&path).expect("reopening the capture");
    let sources: Vec<String> = conn
        .prepare("SELECT DISTINCT pos_source FROM observation")
        .and_then(|mut q| q.query_map([], |r| r.get(0)).and_then(Iterator::collect))
        .expect("reading back the sources");
    assert_eq!(sources, vec!["gps".to_owned()], "the receiver outranks the typed-in position");

    let latitudes: Vec<f64> = conn
        .prepare("SELECT DISTINCT lat FROM observation ORDER BY lat")
        .and_then(|mut q| q.query_map([], |r| r.get(0)).and_then(Iterator::collect))
        .expect("reading back the latitudes");
    assert_eq!(
        latitudes.len(),
        2,
        "rows follow the receiver rather than freezing at the first fix"
    );

    let mut csv = Vec::new();
    let summary = wigle_csv(&conn, &ExportFilter::default(), &mut csv, "0.1.0").expect("exporting");
    let csv = String::from_utf8(csv).expect("the CSV is UTF-8");
    assert!(summary.rows > 0);
    for row in csv.lines().skip(2) {
        let fields: Vec<&str> = row.split(',').collect();
        assert!(fields[7].starts_with("48.1"), "the receiver\'s latitude, not the fallback: {row}");
        // The receiver's own dilution of precision, carried all the way to the
        // column WiGLE reads it from — 0 there means "unknown".
        assert_eq!(fields[10], "4.5", "{row}");
    }
    assert!(
        csv.contains(",76,BLE"),
        "the simulated advertisers carry their manufacturer identifiers: {csv}"
    );
}

#[tokio::test(start_paused = true)]
async fn drive_ends_on_host_row_with_final_counts_when_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    let link = SimTransport::new(SimConfig { node_count: 2, speed: 60.0, ..Default::default() })
        .start()
        .expect("starting the simulator");
    let started = now();
    let store = Store::create(&StoreConfig::new(&path), &CaptureInfo::default(), started.unix_ms)
        .expect("creating the store");
    let engine = FleetEngine::new(EngineConfig::default(), started);
    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (_command_tx, command_rx) = tokio::sync::mpsc::channel(4);

    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    // One sample at the start, two more on the timer, then the one at shutdown.
    tokio::time::sleep(Duration::from_secs(12)).await;
    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    let counters = snapshot_rx.borrow().counters;
    let conn = open_readonly(&path).expect("reopening the capture");
    let rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM host_status", [], |r| r.get(0)).expect("counting");
    assert_eq!(rows, 4, "one at the start, one every 5 s and one at shutdown");
    let last: (i64, i64, i64) = conn
        .query_row(
            "SELECT frames, duplicate_batches, incompatible
             FROM host_status ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("the last host row");
    let expected = (counters.frames, counters.duplicate_batches, counters.incompatible);
    assert_eq!(last, (expected.0 as i64, expected.1 as i64, expected.2 as i64));
    assert!(counters.frames > 0, "the fleet was heard");

    let host_frames: i64 = conn
        .query_row("SELECT MAX(host_frames) FROM bridge_status", [], |r| r.get(0))
        .expect("a bridge status row");
    assert!(host_frames > 0, "status replies carry the frames read so far");
}
