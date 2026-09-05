//! The store and the export, end to end through a real SQLite file.
//!
//! The export's job is to pick one row per network out of many sightings, and
//! to format it the way WiGLE will accept. Both halves are easy to get subtly
//! wrong and impossible to notice from the outside — a file WiGLE rejects looks
//! exactly like a file it accepts until it is uploaded.

use std::time::Duration;

use rusqlite::Connection;
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::position::{Fix, PositionSource};
use wartui_core::record::{Heartbeat, NodeSeen, Observation, Record};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;
use wartui_proto::plan::ChannelPool;

const NODE: Mac = [0x38, 0x44, 0xBE, 0x1F, 0x57, 0x84];
const OTHER: Mac = [0x38, 0x44, 0xBE, 0x1F, 0x57, 0x85];
const EPOCH_MS: i64 = 1_777_642_477_000;

fn fixed(lat: f64, lon: f64) -> Fix {
    Fix {
        lat: Some(lat),
        lon: Some(lon),
        alt: Some(16.0),
        accuracy: None,
        source: PositionSource::Static,
        at_ms: None,
    }
}

fn observation(node: Mac, bssid: [u8; 6], rssi: i16, at_ms: i64, fix: Fix) -> Record {
    Record::Observation(Observation {
        node_mac: node,
        rx_at_ms: at_ms,
        link_rssi: Some(-41),
        bssid,
        ssid: b"example".to_vec(),
        security: "[WPA2_PSK]".to_owned(),
        channel: 6,
        rssi,
        kind: RecordKind::Wifi,
        fix,
        raw_text: b"raw".to_vec(),
    })
}

/// A store on a fresh temporary database, plus the directory keeping it alive.
fn store(dir: &tempfile::TempDir) -> Store {
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    // Small and quick, so a test does not sit waiting for a batch window.
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(10);
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, ..Default::default() };
    Store::open(&config, &session, EPOCH_MS).expect("opening the store")
}

/// Write records and close, which is what commits the final batch.
fn write(dir: &tempfile::TempDir, records: Vec<Record>) -> Connection {
    let store = store(dir);
    assert_eq!(store.submit(records), 0, "nothing should have been dropped");
    store.close();
    open_readonly(&dir.path().join("wartui.db")).expect("reopening read-only")
}

fn export(conn: &Connection) -> (String, wartui_core::export::ExportSummary) {
    let mut out = Vec::new();
    let summary = wigle_csv(conn, ExportFilter::default(), &mut out, "0.1.0").expect("exporting");
    (String::from_utf8(out).expect("the CSV is UTF-8"), summary)
}

#[test]
fn every_kind_of_record_round_trips() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            Record::Node(NodeSeen { mac: NODE, first_seen_ms: EPOCH_MS, last_seen_ms: EPOCH_MS }),
            Record::Heartbeat(Heartbeat {
                node_mac: NODE,
                rx_at_ms: EPOCH_MS,
                counter: 174,
                link_rssi: Some(-41),
            }),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.7749, -122.4194)),
        ],
    );

    let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM node", [], |r| r.get(0)).unwrap();
    let beats: i64 = conn.query_row("SELECT COUNT(*) FROM heartbeat", [], |r| r.get(0)).unwrap();
    let obs: i64 = conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).unwrap();
    assert_eq!((nodes, beats, obs), (1, 1, 1));

    let (mac, source): (Vec<u8>, String) = conn
        .query_row("SELECT node_mac, pos_source FROM observation", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(mac, NODE.to_vec(), "MACs are stored as six raw bytes, never as text");
    assert_eq!(source, "static");
}

#[test]
fn seeing_a_node_again_updates_last_seen_without_moving_first_seen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            Record::Node(NodeSeen { mac: NODE, first_seen_ms: EPOCH_MS, last_seen_ms: EPOCH_MS }),
            Record::Node(NodeSeen {
                mac: NODE,
                first_seen_ms: EPOCH_MS,
                last_seen_ms: EPOCH_MS + 60_000,
            }),
        ],
    );

    let (first, last): (i64, i64) = conn
        .query_row("SELECT first_seen, last_seen FROM node", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((first, last), (EPOCH_MS, EPOCH_MS + 60_000));
}

#[test]
fn every_sighting_is_kept_rather_than_deduplicated_on_the_way_in() {
    // Two nodes seeing one access point is coverage data, not a duplicate. The
    // export is where one row per network gets chosen.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -70, EPOCH_MS, fixed(37.0, -122.0)),
            observation(OTHER, [0xAA; 6], -50, EPOCH_MS + 1000, fixed(38.0, -123.0)),
        ],
    );

    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 2);
}

#[test]
fn the_export_picks_the_strongest_sighting_but_the_earliest_first_seen() {
    // The strongest signal is the sighting whose position is closest to the
    // transmitter, so that is the row that goes out. `FirstSeen` is a different
    // question and comes from a different row, which is why this is a window
    // query rather than a `GROUP BY`.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -70, EPOCH_MS, fixed(37.0, -122.0)),
            observation(OTHER, [0xAA; 6], -50, EPOCH_MS + 60_000, fixed(38.5, -123.5)),
        ],
    );

    let (csv, summary) = export(&conn);
    let row = csv.lines().nth(2).expect("one data row");

    assert_eq!(summary.networks, 1);
    assert!(row.contains("38.5,-123.5"), "the strongest sighting's position: {row}");
    assert!(row.contains(",-50,"), "and its RSSI: {row}");
    assert!(row.starts_with("AA:AA:AA:AA:AA:AA,"));
    assert!(row.contains("2026-05-01 13:34:37"), "the earliest sighting's time: {row}");
}

#[test]
fn the_header_is_the_wigle_v1_4_pair() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(&dir, vec![]);
    let (csv, _) = export(&conn);
    let mut lines = csv.lines();

    assert!(lines.next().expect("pre-header").starts_with("WigleWifi-1.4,appRelease=0.1.0,"));
    assert_eq!(
        lines.next().expect("column header"),
        "MAC,SSID,AuthMode,FirstSeen,Channel,RSSI,\
         CurrentLatitude,CurrentLongitude,AltitudeMeters,AccuracyMeters,Type"
    );
}

#[test]
fn a_network_nobody_had_a_position_for_is_counted_rather_than_written() {
    // Not an error and not silent: WiGLE cannot use a row without coordinates,
    // but the operator needs to know how much of the capture is waiting on a
    // `--lat`/`--lon` before uploading.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xBB; 6], -60, EPOCH_MS, Fix::none()),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary, wartui_core::export::ExportSummary { networks: 1, unpositioned: 1 });
    assert!(!csv.contains("BB:BB:BB:BB:BB:BB"));
}

#[test]
fn a_network_with_one_positioned_sighting_is_exported_from_that_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            // The strongest sighting has no position, so it cannot be the one
            // submitted; the weaker positioned one still can.
            observation(NODE, [0xAA; 6], -40, EPOCH_MS, Fix::none()),
            observation(NODE, [0xAA; 6], -80, EPOCH_MS + 1000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary, wartui_core::export::ExportSummary { networks: 1, unpositioned: 0 });
    assert!(csv.lines().nth(2).expect("a row").contains(",-80,37,-122,"));
}

#[test]
fn an_ssid_with_a_comma_or_a_quote_is_rfc_4180_quoted() {
    // The node firmware replaces commas in SSIDs with underscores, but the
    // export must not depend on that: the store also holds text from older
    // captures and from firmware nobody here controls.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut awkward = observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut awkward else { unreachable!() };
    obs.ssid = br#"cafe, "the" one"#.to_vec();
    let conn = write(&dir, vec![awkward]);

    let (csv, _) = export(&conn);
    assert!(
        csv.contains(r#""cafe, ""the"" one""#),
        "the SSID should be quoted and its quotes doubled: {csv}"
    );
}

#[test]
fn a_ble_record_exports_with_the_type_wigle_expects() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ble = observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut ble else { unreachable!() };
    obs.kind = RecordKind::Ble;
    obs.channel = 0;
    obs.ssid = Vec::new();
    obs.security = "[BLE]".to_owned();
    let conn = write(&dir, vec![ble]);

    let (csv, _) = export(&conn);
    let row = csv.lines().nth(2).expect("a row");
    assert!(row.ends_with(",BLE"), "{row}");
    assert!(row.contains("AA:AA:AA:AA:AA:AA,,[BLE],"), "an empty SSID stays empty: {row}");
}

#[test]
fn a_full_queue_drops_and_counts_rather_than_blocking_the_engine() {
    // A stalled engine misses everything, including — once Phase 4 lands — the
    // assignment racing a node's 300 ms window. One lost observation is the
    // cheaper failure, but it has to be visible.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.queue_depth = 1;
    config.batch_rows = 1024;
    config.batch_interval = Duration::from_secs(3600);
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, ..Default::default() };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

    let flood: Vec<Record> =
        (0..2048).map(|n| observation(NODE, [n as u8; 6], -60, EPOCH_MS, Fix::none())).collect();
    let dropped = store.submit(flood);

    assert!(dropped > 0, "a depth-1 queue and no draining should overflow");
    assert_eq!(store.stats().dropped, dropped as u64, "and say so in the stats");
}
