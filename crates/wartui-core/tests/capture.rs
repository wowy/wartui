//! The whole chain, with a simulated fleet standing in for hardware.
//!
//! Every piece below is covered on its own elsewhere. This is the test that the
//! pieces are actually connected: that a frame arriving on the link becomes a
//! row in SQLite becomes a line in a file WiGLE would accept, without anyone
//! having to plug anything in.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{Command, EngineConfig, FleetEngine, StoreStats};
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::gps::Gps;
use wartui_core::position::PositionChain;
use wartui_core::runtime::{drive, now};
use wartui_core::store::{CaptureInfo, Store, StoreConfig, open_readonly};

#[tokio::test(start_paused = true)]
async fn capture_run_persists_sightings_and_exports_wigle_when_simulated() {
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
    assert_eq!(snapshot.counters.undecodable, 0);
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

    let mut csv = Vec::new();
    let summary = wigle_csv(&conn, ExportFilter::default(), &mut csv, "0.1.0").expect("exporting");
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

/// Two fixes a few streets apart, so a row can be told which one it was written
/// under. Both are somewhere in Munich; the static fallback below is in San
/// Francisco, which makes the tier that answered obvious from the coordinates.
const FIRST: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
const SECOND: &[u8] = b"$GPGGA,123529.00,4810.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*6C";

#[tokio::test(start_paused = true)]
async fn capture_run_attaches_dynamic_gps_positions_when_vehicle_moves() {
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
        // A static position underneath, to prove the receiver is what the rows
        // actually used rather than the only thing they could have used.
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0))
            .with_gps(gps.clone(), Duration::from_secs(60)),
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
    command_tx
        .send(Command::AssignBle { mac: Some(SimTransport::node_mac(0)) })
        .await
        .expect("the engine is listening");
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
    let summary = wigle_csv(&conn, ExportFilter::default(), &mut csv, "0.1.0").expect("exporting");
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
async fn capture_run_ends_on_host_row_holding_final_counts_when_stopped() {
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
            "SELECT frames, duplicate_batches, admin_windows_missed
             FROM host_status ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("the last host row");
    let expected = (counters.frames, counters.duplicate_batches, counters.admin_windows_missed);
    assert_eq!(last, (expected.0 as i64, expected.1 as i64, expected.2 as i64));
    assert!(counters.frames > 0, "the fleet was heard");

    let host_frames: i64 = conn
        .query_row("SELECT MAX(host_frames) FROM bridge_status", [], |r| r.get(0))
        .expect("a bridge status row");
    assert!(host_frames > 0, "status replies carry the frames read so far");
}
