//! The store and the export, end to end through a real SQLite file.
//!
//! The export's job is to fold a network's many sightings into recapture
//! windows and pick one row per window, and to format it the way WiGLE will
//! accept. Both halves are easy to get subtly wrong and impossible to notice
//! from the outside — a file WiGLE rejects looks exactly like a file it
//! accepts until it is uploaded.

use std::time::Duration;

use rusqlite::Connection;
use wartui_core::engine::{EngineConfig, FleetEngine, Now, StorePeaks};
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::nmea::accuracy_from_hdop;
use wartui_core::position::{Fix, PositionSource};
use wartui_core::record::{
    AdminOutcome, AssignmentSent, BatchGap, BridgeSeen, BridgeStatusSeen, Heartbeat, HostStatus,
    NodeSeen, Observation, Record,
};
use wartui_core::store::{
    CaptureInfo, CaptureProvenance, Checkpoint, SCHEMA_VERSION, Store, StoreConfig, StoreError,
    is_simulated, open_readonly, open_readwrite, record_upload, set_upload_result,
};
use wartui_proto::air::RecordKind;
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelPool;

const NODE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
const OTHER: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x85];
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

/// Block until `done`, or fail saying what never happened.
///
/// The store's threads are what these tests are watching, and how long one takes is the
/// machine's business rather than the store's: a sleep long enough on a workstation is a
/// coin toss on a loaded CI runner. The cap is only here so a genuine stall fails instead
/// of hanging.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    const CAP: Duration = Duration::from_secs(10);
    let until = std::time::Instant::now() + CAP;
    while !done() {
        assert!(std::time::Instant::now() < until, "waited {CAP:?} for {what}");
        std::thread::sleep(Duration::from_millis(1));
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
        rcoi: None,
        mfgr_id: None,
        fix,
        raw_body: b"raw".to_vec(),
    })
}

/// A Bluetooth sighting, otherwise as [`observation`].
fn ble_observation(node: Mac, bssid: [u8; 6], at_ms: i64, fix: Fix) -> Record {
    let mut record = observation(node, bssid, -60, at_ms, fix);
    let Record::Observation(obs) = &mut record else { unreachable!() };
    obs.kind = RecordKind::Ble;
    obs.channel = 0;
    record
}

/// A Wi-Fi sighting on `channel`, otherwise as [`observation`].
fn on_channel(bssid: [u8; 6], channel: u16, at_ms: i64) -> Record {
    let mut record = observation(NODE, bssid, -60, at_ms, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut record else { unreachable!() };
    obs.channel = channel;
    record
}

/// A store on a fresh temporary database, plus the directory keeping it alive.
fn store(dir: &tempfile::TempDir) -> Store {
    open_at(&dir.path().join("wartui.db"))
}

/// A store on one named path, so a test can read the same database back.
fn open_at(path: &std::path::Path) -> Store {
    let mut config = StoreConfig::new(path);
    // Small and quick, so a test does not sit waiting for a batch window.
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(10);
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    Store::create(&config, &info, EPOCH_MS).expect("creating the store")
}

#[test]
fn store_records_scan_table_when_capture_created() {
    let dir = tempfile::tempdir().expect("temp dir");
    store(&dir).close();
    let conn = open_readonly(&dir.path().join("wartui.db")).expect("read-only capture");
    let channels: String = conn
        .query_row("SELECT v FROM kv WHERE k = 'capture.scan_channels'", [], |r| r.get(0))
        .expect("ordered scan table provenance");
    let expected =
        wartui_proto::plan::SCAN_CHANNELS.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    assert_eq!(channels, expected);
    let version: String = conn
        .query_row("SELECT v FROM kv WHERE k = 'capture.wire_version'", [], |r| r.get(0))
        .expect("wire version provenance");
    assert_eq!(version, wartui_proto::air::WIRE_VERSION.to_string());
}

#[test]
fn store_records_initial_provenance_when_capture_created() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("provenance.db");
    let engine = FleetEngine::new(
        EngineConfig { tx_power: 127, bridge_tx_power: -1, record_raw: true, ..Default::default() },
        Now { mono: std::time::Instant::now(), unix_ms: EPOCH_MS },
    );
    let provenance = CaptureProvenance {
        host_version: Some("0.1.0".to_owned()),
        host_build: Some("fnv1a64:0123456789abcdef".to_owned()),
        release_tag: Some("v-test".to_owned()),
        initial_settings: Some(engine.capture_settings()),
    };
    let store = Store::create_with_provenance(
        &StoreConfig::new(&path),
        &CaptureInfo::default(),
        EPOCH_MS,
        &provenance,
    )
    .expect("capture with provenance");
    // Metadata must be committed before the writer has flushed any records.
    let conn = open_readonly(&path).expect("open while writer is running");
    let get = |key| {
        conn.query_row("SELECT v FROM kv WHERE k = ?1", [key], |r| r.get::<_, String>(0))
            .expect("metadata")
    };
    assert_eq!(get("capture.host_version"), "0.1.0");
    assert_eq!(get("capture.host_build"), "fnv1a64:0123456789abcdef");
    assert_eq!(get("capture.release_tag"), "v-test");
    let settings = get("capture.settings.initial");
    assert!(settings.contains(&format!("at_ms={EPOCH_MS};")), "{settings}");
    assert!(settings.contains("nodes_tx_power_quarter_dbm=80;"), "{settings}");
    assert!(settings.contains("bridge_tx_power_quarter_dbm=8;"), "{settings}");
    assert!(settings.contains("record_raw=true;gps=false;gps_max_age_ms=5000;"), "{settings}");
    assert!(settings.contains("preferred_ble=none;ble_node=none;"), "{settings}");
    assert_eq!(store.close().dropped, 0);
}

#[test]
fn store_preserves_settings_order_when_timestamps_repeat_or_reverse() {
    let dir = tempfile::tempdir().expect("temp dir");
    let engine = FleetEngine::new(
        EngineConfig::default(),
        Now { mono: std::time::Instant::now(), unix_ms: EPOCH_MS },
    );
    let settings = engine.capture_settings();
    let records = [EPOCH_MS, EPOCH_MS, EPOCH_MS - 1]
        .into_iter()
        .enumerate()
        .map(|(sequence, at_ms)| Record::CaptureSettings {
            sequence: sequence as u64,
            at_ms,
            settings: wartui_core::record::CaptureSettings {
                tx_power: 8 + sequence as i8,
                ..settings
            },
        })
        .collect();
    let conn = write(&dir, records);
    let mut stmt = conn
        .prepare("SELECT k, v FROM kv WHERE k LIKE 'capture.settings.%' ORDER BY k")
        .expect("settings query");
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("settings rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("metadata strings");
    assert_eq!(rows.len(), 3);
    for (i, (key, value)) in rows.iter().enumerate() {
        assert_eq!(key, &format!("capture.settings.{i:020}"));
        assert!(value.contains(&format!("nodes_tx_power_quarter_dbm={};", 8 + i)), "{value}");
    }
    assert!(rows[2].1.contains(&format!("at_ms={};", EPOCH_MS - 1)));
    let unknown: i64 = conn
        .query_row("SELECT COUNT(*) FROM kv WHERE k = 'capture.host_build'", [], |r| r.get(0))
        .expect("unknown host identity");
    assert_eq!(unknown, 0, "a core-created capture must not invent executable identity");
}

/// Write records and close, which is what commits the final batch.
fn write(dir: &tempfile::TempDir, records: Vec<Record>) -> Connection {
    let store = store(dir);
    assert_eq!(store.submit(records), 0, "nothing should have been dropped");
    store.close();
    open_readonly(&dir.path().join("wartui.db")).expect("reopening read-only")
}

fn export(conn: &Connection) -> (String, wartui_core::export::ExportSummary) {
    export_with(conn, ExportFilter::default())
}

/// Export with an explicit filter, for the tests that pin a window of their
/// own rather than the default one.
fn export_with(
    conn: &Connection,
    filter: ExportFilter,
) -> (String, wartui_core::export::ExportSummary) {
    let mut out = Vec::new();
    let summary = wigle_csv(conn, filter, &mut out, "0.1.0").expect("exporting");
    (String::from_utf8(out).expect("the CSV is UTF-8"), summary)
}

#[test]
fn store_records_pool_spelling_when_capture_created() {
    // Lowercase, and deliberately not `ChannelPool`'s `Display`: captures on
    // disk carry these strings, so the two spellings are separate on purpose.
    for (pool, spelling) in
        [(ChannelPool::Us, "us"), (ChannelPool::Eu, "eu"), (ChannelPool::All, "all")]
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wartui.db");
        let config = StoreConfig::new(&path);
        let info = CaptureInfo { pool, notes: None, simulated: false };
        Store::create(&config, &info, EPOCH_MS).expect("creating the store").close();

        let conn = open_readonly(&path).expect("reopening read-only");
        let stored: String = conn
            .query_row("SELECT channel_pool FROM capture", [], |row| row.get(0))
            .expect("the capture row");
        assert_eq!(stored, spelling, "{pool:?}");
    }
}

#[test]
fn store_records_simulated_flag_when_capture_created() {
    for simulated in [false, true] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wartui.db");
        let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated };
        Store::create(&StoreConfig::new(&path), &info, EPOCH_MS)
            .expect("creating the store")
            .close();

        let conn = open_readonly(&path).expect("reopening read-only");
        assert_eq!(is_simulated(&conn).expect("the capture row"), simulated);
    }
}

#[test]
fn store_round_trips_record_when_each_type_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            Record::Node(NodeSeen {
                mac: NODE,
                first_seen_ms: EPOCH_MS,
                last_seen_ms: EPOCH_MS,
                capabilities: Some("wartui/0.1;5g".to_owned()),
            }),
            Record::Heartbeat(Heartbeat {
                node_mac: NODE,
                rx_at_ms: EPOCH_MS,
                counter: 174,
                epoch: 5,
                link_rssi: Some(-41),
                wifi_dropped: 12,
                ble_dropped: 3,
                beat: 61,
                unsent: 4,
                live: false,
            }),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.7749, -122.4194)),
        ],
    );

    let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM node", [], |r| r.get(0)).unwrap();
    let beats: i64 = conn.query_row("SELECT COUNT(*) FROM heartbeat", [], |r| r.get(0)).unwrap();
    let obs: i64 = conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).unwrap();
    assert_eq!((nodes, beats, obs), (1, 1, 1));

    let epoch: i64 = conn.query_row("SELECT epoch FROM heartbeat", [], |r| r.get(0)).unwrap();
    assert_eq!(epoch, 5, "the epoch the frame said the node held");

    let dropped: (i64, i64) = conn
        .query_row("SELECT wifi_dropped, ble_dropped FROM heartbeat", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(dropped, (12, 3), "the since-boot counts as the frame carried them");

    let beat: i64 = conn.query_row("SELECT beat FROM heartbeat", [], |r| r.get(0)).unwrap();
    assert_eq!(beat, 61, "the since-boot heartbeat count as the frame carried it");

    let unsent: i64 = conn.query_row("SELECT unsent FROM heartbeat", [], |r| r.get(0)).unwrap();
    assert_eq!(unsent, 4, "the since-boot refused-send count as the frame carried it");

    let live: bool = conn.query_row("SELECT live FROM heartbeat", [], |r| r.get(0)).unwrap();
    assert!(!live, "replayed from the bridge's backlog");

    let (mac, source): (Vec<u8>, String) = conn
        .query_row("SELECT node_mac, pos_source FROM observation", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(mac, NODE.to_vec(), "MACs are stored as six raw bytes, never as text");
    assert_eq!(source, "static");
}

/// Asserts that the one row in `table` has no NULL column and that no column declares a default.
/// A column the insert omits shows up as NULL or as its default, so together they prove the
/// insert names every column.
fn assert_insert_names_every_column(conn: &Connection, table: &str) {
    let columns: Vec<String> = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!columns.is_empty(), "the {table} table has columns");
    for column in &columns {
        let null: bool = conn
            .query_row(&format!("SELECT \"{column}\" IS NULL FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert!(!null, "the {table} insert leaves `{column}` NULL");
    }

    let defaulted: Vec<String> = conn
        .prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') WHERE dflt_value IS NOT NULL"
        ))
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(defaulted.is_empty(), "{table} columns declare a default: {defaulted:?}");
}

#[test]
fn store_writes_every_heartbeat_column_when_heartbeat_recorded() {
    // The record sets every field, so any column the insert omits shows up as NULL or as a
    // default.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![Record::Heartbeat(Heartbeat {
            node_mac: NODE,
            rx_at_ms: EPOCH_MS,
            counter: 174,
            epoch: 5,
            link_rssi: Some(-41),
            wifi_dropped: 12,
            ble_dropped: 3,
            beat: 61,
            unsent: 4,
            live: true,
        })],
    );
    assert_insert_names_every_column(&conn, "heartbeat");
}

#[test]
fn store_writes_every_node_column_when_node_recorded() {
    // The record sets every field, so any column the insert omits shows up as NULL or as a
    // default. `capabilities` is NULL for a node heard only by observation, so the record sets it.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![Record::Node(NodeSeen {
            mac: NODE,
            first_seen_ms: EPOCH_MS,
            last_seen_ms: EPOCH_MS,
            capabilities: Some("wartui/0.1;5g".to_owned()),
        })],
    );
    assert_insert_names_every_column(&conn, "node");
}

#[test]
fn store_round_trips_every_column_when_bridge_status_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![Record::BridgeStatus(BridgeStatusSeen {
            rx_at_ms: EPOCH_MS + 5_000,
            peer_count: 3,
            rx_count: 4_000_000_000,
            dropped_tx: 1305,
            uptime_ms: 3_240_000,
            host_frames: 3_999_999_000,
        })],
    );

    let row: (i64, i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT rx_at, peer_count, rx_count, dropped_tx, uptime_ms, host_frames
             FROM bridge_status",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(row, (EPOCH_MS + 5_000, 3, 4_000_000_000, 1305, 3_240_000, 3_999_999_000));
}

fn host_status(
    throttled: Option<u32>,
    soc_temp_mc: Option<i32>,
    battery_mv: Option<i32>,
    battery_ma: Option<i32>,
) -> HostStatus {
    HostStatus {
        at_ms: EPOCH_MS + 5_000,
        frames: 1,
        duplicate_batches: 2,
        garbled: 3,
        undecodable: 4,
        incompatible: 5,
        foreign_fleet: 6,
        foreign_admin: 7,
        admin_windows_missed: 8,
        lag_peak_us: 9,
        store_written: 10,
        store_dropped: 11,
        store_queue_peak: 12,
        store_commit_peak_us: 13,
        throttled,
        soc_temp_mc,
        battery_mv,
        battery_ma,
    }
}

#[test]
fn store_round_trips_every_column_when_host_status_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            Record::HostStatus(host_status(Some(0x50005), Some(61_234), Some(4_185), Some(-21))),
            Record::HostStatus(host_status(None, None, None, None)),
        ],
    );

    let mut stmt = conn
        .prepare(
            "SELECT at, frames, duplicate_batches, garbled, undecodable, incompatible,
                    foreign_fleet, foreign_admin, admin_windows_missed, lag_peak_us,
                    store_written, store_dropped, store_queue_peak, store_commit_peak_us,
                    throttled, soc_temp_mc, battery_mv, battery_ma
             FROM host_status ORDER BY id",
        )
        .unwrap();
    let rows: Vec<HostStatus> = stmt
        .query_map([], |r| {
            let count = |i| r.get::<_, i64>(i).map(|v| u64::try_from(v).unwrap());
            Ok(HostStatus {
                at_ms: r.get(0)?,
                frames: count(1)?,
                duplicate_batches: count(2)?,
                garbled: count(3)?,
                undecodable: count(4)?,
                incompatible: count(5)?,
                foreign_fleet: count(6)?,
                foreign_admin: count(7)?,
                admin_windows_missed: count(8)?,
                lag_peak_us: count(9)?,
                store_written: count(10)?,
                store_dropped: count(11)?,
                store_queue_peak: count(12)?,
                store_commit_peak_us: count(13)?,
                throttled: r.get(14)?,
                soc_temp_mc: r.get(15)?,
                battery_mv: r.get(16)?,
                battery_ma: r.get(17)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            host_status(Some(0x50005), Some(61_234), Some(4_185), Some(-21)),
            host_status(None, None, None, None)
        ],
        "NULL health reads back as None"
    );
}

#[test]
fn store_round_trips_every_column_when_batch_gap_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![Record::BatchGap(BatchGap {
            node_mac: NODE,
            rx_at_ms: EPOCH_MS + 7_000,
            after_seq: 65535,
            seq: 2,
            lost: 2,
        })],
    );

    let row: (Vec<u8>, i64, i64, i64, i64) = conn
        .query_row("SELECT node_mac, rx_at, after_seq, seq, lost FROM batch_gap", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap();
    assert_eq!(row, (NODE.to_vec(), EPOCH_MS + 7_000, 65535, 2, 2));
}

#[test]
fn store_keeps_first_seen_and_token_when_node_seen_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            Record::Node(NodeSeen {
                mac: NODE,
                first_seen_ms: EPOCH_MS,
                last_seen_ms: EPOCH_MS,
                capabilities: Some("wartui/0.1;5g".to_owned()),
            }),
            // The row that would erase the identity if the upsert wrote
            // `excluded.capabilities` straight in.
            Record::Node(NodeSeen {
                mac: NODE,
                first_seen_ms: EPOCH_MS,
                last_seen_ms: EPOCH_MS + 60_000,
                capabilities: None,
            }),
        ],
    );

    let (first, last, caps): (i64, i64, Option<String>) = conn
        .query_row("SELECT first_seen, last_seen, capabilities FROM node", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((first, last), (EPOCH_MS, EPOCH_MS + 60_000));
    assert_eq!(caps.as_deref(), Some("wartui/0.1;5g"), "what it last said it was, kept");
}

#[test]
fn store_keeps_every_sighting_when_bssid_repeats() {
    // Two nodes seeing one access point is coverage data, not a duplicate.
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
fn wigle_csv_takes_strongest_sighting_and_earliest_first_seen_when_heard_twice() {
    // The strongest signal is the sighting closest to the transmitter, and
    // `FirstSeen` comes from a different row — hence the window query.
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

    assert_eq!(summary.rows, 1);
    assert!(row.contains("38.5,-123.5"), "the strongest sighting's position: {row}");
    assert!(row.contains(",-50,"), "and its RSSI: {row}");
    assert!(row.starts_with("AA:AA:AA:AA:AA:AA,"));
    assert!(row.contains("2026-05-01 13:34:37"), "the earliest sighting's time: {row}");
}

#[test]
fn wigle_csv_writes_pre_header_and_header_when_exporting() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(&dir, vec![]);
    let (csv, _) = export(&conn);
    let mut lines = csv.lines();

    let pre_header = lines.next().expect("pre-header");
    assert!(pre_header.starts_with("WigleWifi-1.6,appRelease=0.1.0,"));
    assert!(pre_header.ends_with("star=Sol,body=3,subBody=0"), "{pre_header}");
    assert_eq!(
        lines.next().expect("column header"),
        "MAC,SSID,AuthMode,FirstSeen,Channel,Frequency,RSSI,\
         CurrentLatitude,CurrentLongitude,AltitudeMeters,AccuracyMeters,RCOIs,MfgrId,Type"
    );
}

#[test]
fn wigle_csv_counts_unpositioned_when_no_fix() {
    // Not an error and not silent: the operator needs to know how much of a capture
    // is waiting on a GPS before uploading.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xBA; 6], -60, EPOCH_MS, Fix::none()),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!((summary.rows, summary.unpositioned), (1, 1));
    assert!(!csv.contains("BA:BA:BA:BA:BA:BA"));
}

#[test]
fn wigle_csv_prefers_positioned_sighting_when_stronger_one_unfixed() {
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
    assert_eq!((summary.rows, summary.unpositioned), (1, 0));
    assert!(csv.lines().nth(2).expect("a row").contains(",-80,37,-122,"));
}

#[test]
fn wigle_csv_quotes_ssid_when_it_holds_comma_or_quote() {
    // The node firmware replaces commas in SSIDs, but the export must not depend on
    // that: the store also holds text from older captures.
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
fn wigle_csv_writes_accuracy_without_float_noise() {
    // Accuracies from the parser's own conversion, so the noise is real: HDOP 1.32 gives
    // 6.6000000000000005 and 0.53 gives 2.6500000000000004 in f64.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut first = fixed(37.0, -122.0);
    first.accuracy = Some(accuracy_from_hdop(1.32));
    let mut second = fixed(37.0, -122.0);
    second.accuracy = Some(accuracy_from_hdop(0.53));
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, first),
            observation(NODE, [0xCC; 6], -60, EPOCH_MS, second),
        ],
    );

    let (csv, _) = export(&conn);
    // `AccuracyMeters` is the eleventh column; no field before it holds a comma here.
    let accuracy = |bssid: &str| {
        let row = csv.lines().find(|line| line.starts_with(bssid)).expect("a row");
        row.split(',').nth(10).expect("an AccuracyMeters column").to_owned()
    };
    assert_eq!(accuracy("AA:AA:AA:AA:AA:AA"), "6.6");
    assert_eq!(accuracy("CC:CC:CC:CC:CC:CC"), "2.65");
}

#[test]
fn wigle_csv_writes_coordinates_to_seven_decimals_when_fix_from_nmea_minutes() {
    // A stored fix from the 2026-10-07 drive: degrees plus minutes over sixty, as the parser
    // computes them, with more digits than the receiver resolves.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![observation(
            NODE,
            [0xAA; 6],
            -60,
            EPOCH_MS,
            fixed(45.02772083333333, -93.80174716666667),
        )],
    );

    let (csv, _) = export(&conn);
    let row = csv.lines().find(|line| line.starts_with("AA:AA:AA:AA:AA:AA")).expect("a row");
    // `CurrentLatitude` and `CurrentLongitude` are the eighth and ninth columns; no field before
    // them holds a comma here.
    let columns: Vec<&str> = row.split(',').collect();
    assert_eq!((columns[7], columns[8]), ("45.0277208", "-93.8017472"), "{row}");
}

#[test]
fn wigle_csv_strips_padding_when_stored_ssid_cloaked() {
    // What a node flashed before `beacon::visible_ssid` existed put in the file: the
    // name's real length, every byte zero.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cloaked = observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut cloaked else { unreachable!() };
    obs.ssid = vec![0u8; 8];
    let conn = write(&dir, vec![cloaked]);

    let (csv, _) = export(&conn);
    let row = csv.lines().nth(2).expect("a row");
    assert!(row.contains("AA:AA:AA:AA:AA:AA,,["), "a cloaked SSID exports as hidden: {row:?}");
    assert!(!csv.contains('\0'), "no NUL may reach the file: {csv:?}");
}

#[test]
fn wigle_csv_writes_blank_frequency_when_sighting_ble() {
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
    assert!(row.ends_with(",,,BLE"), "{row}");
    assert!(row.contains("AA:AA:AA:AA:AA:AA,,[BLE],"), "an empty SSID stays empty: {row}");
    // Channel 0 with a blank frequency behind it: a passive scan has no
    // "device type" code to put there, and the row says so by leaving it out.
    assert!(row.contains(",0,,-60,"), "channel 0, then no frequency: {row}");
}

/// The OpenRoaming triple, the widest roaming consortium element anybody real
/// beacons.
const OPEN_ROAMING: [u8; 17] = [
    0x02, 0x55, //
    0x5A, 0x03, 0xBA, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x20, 0x00,
];

#[test]
fn wigle_csv_writes_rcois_when_access_point_is_passpoint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut passpoint = observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut passpoint else { unreachable!() };
    obs.rcoi = Some(OPEN_ROAMING.to_vec());
    let conn = write(&dir, vec![passpoint]);

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 1);
    assert!(
        csv.contains("5A03BA0000 BAA2D00000 BAA2D02000,,WIFI"),
        "the identifiers in WiGLE's spelling, and no manufacturer id: {csv}"
    );
}

#[test]
fn wigle_csv_keeps_rcois_when_strongest_lacks_them() {
    // A beacon without the element outshouts a weaker one with it. The row's
    // position and signal are the strong sighting's; its identifiers are the
    // window's.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut weak = observation(NODE, [0xAA; 6], -80, EPOCH_MS, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut weak else { unreachable!() };
    obs.rcoi = Some(OPEN_ROAMING.to_vec());
    let strong = observation(OTHER, [0xAA; 6], -40, EPOCH_MS + 1000, fixed(38.0, -123.0));
    let conn = write(&dir, vec![weak, strong]);

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 1);
    let row = csv.lines().nth(2).expect("a row");
    assert!(row.contains(",-40,38,-123,"), "the strong sighting submits: {row}");
    assert!(
        row.ends_with("5A03BA0000 BAA2D00000 BAA2D02000,,WIFI"),
        "and the weak one's identifiers ride along: {row}"
    );
}

#[test]
fn wigle_csv_keeps_mfgr_id_when_only_weaker_later_sighting_carries_it() {
    // The other order: the best sighting is already held when a weaker
    // advertisement with the manufacturer data arrives.
    let dir = tempfile::tempdir().expect("temp dir");
    let ble = |node, rssi, at_ms, mfgr_id| {
        let mut record = observation(node, [0xAA; 6], rssi, at_ms, fixed(37.0, -122.0));
        let Record::Observation(obs) = &mut record else { unreachable!() };
        obs.kind = RecordKind::Ble;
        obs.channel = 0;
        obs.ssid = Vec::new();
        obs.security = "[BLE]".to_owned();
        obs.mfgr_id = mfgr_id;
        record
    };
    let conn = write(
        &dir,
        vec![ble(NODE, -40, EPOCH_MS, None), ble(OTHER, -80, EPOCH_MS + 1000, Some(76))],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 1);
    assert!(csv.contains(",0,,-40,37,-122,16,0,,76,BLE"), "{csv}");
}

#[test]
fn wigle_csv_writes_mfgr_id_when_ble_advertiser_carries_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ble = observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0));
    let Record::Observation(obs) = &mut ble else { unreachable!() };
    obs.kind = RecordKind::Ble;
    obs.channel = 0;
    obs.ssid = Vec::new();
    obs.security = "[BLE]".to_owned();
    obs.mfgr_id = Some(76);
    let conn = write(&dir, vec![ble]);

    let (csv, _) = export(&conn);
    assert!(
        csv.contains(",0,,-60,37,-122,16,0,,76,BLE"),
        "no frequency and no consortium, and the identifier as a number: {csv}"
    );
}

/// `record` as a BLE advertisement: no channel, no SSID, and the `[BLE]` security tag.
fn as_ble(mut record: Record, mfgr_id: Option<u16>) -> Record {
    let Record::Observation(obs) = &mut record else { unreachable!() };
    obs.kind = RecordKind::Ble;
    obs.channel = 0;
    obs.ssid = Vec::new();
    obs.security = "[BLE]".to_owned();
    obs.mfgr_id = mfgr_id;
    record
}

#[test]
fn wigle_csv_writes_row_per_kind_when_address_is_both() {
    // One address can be a Wi-Fi network and a BLE advertiser at once; WiGLE
    // records each, so neither may win the other's window.
    let dir = tempfile::tempdir().expect("temp dir");
    let wifi = observation(NODE, [0xAA; 6], -50, EPOCH_MS, fixed(37.0, -122.0));
    let ble = as_ble(observation(NODE, [0xAA; 6], -70, EPOCH_MS + 3000, fixed(37.0, -122.0)), None);
    let conn = write(&dir, vec![wifi, ble]);

    let (csv, summary) = export(&conn);
    let rows: Vec<&str> = csv.lines().skip(2).collect();
    assert_eq!(summary.rows, 2);
    assert_eq!(rows.len(), 2, "{csv}");
    assert!(rows.iter().any(|row| row.ends_with(",WIFI")), "{csv}");
    assert!(rows.iter().any(|row| row.ends_with(",BLE")), "{csv}");
}

#[test]
fn wigle_csv_keeps_identifiers_within_kind_when_address_is_both() {
    let dir = tempfile::tempdir().expect("temp dir");
    let wifi = observation(NODE, [0xAA; 6], -50, EPOCH_MS, fixed(37.0, -122.0));
    let ble =
        as_ble(observation(NODE, [0xAA; 6], -70, EPOCH_MS + 3000, fixed(37.0, -122.0)), Some(76));
    let conn = write(&dir, vec![wifi, ble]);

    let (csv, _) = export(&conn);
    let row = |kind: &str| {
        csv.lines().find(|row| row.ends_with(kind)).unwrap_or_else(|| panic!("{kind}: {csv}"))
    };
    assert!(row(",WIFI").ends_with(",,,WIFI"), "no BLE identifier on the Wi-Fi row: {csv}");
    assert!(row(",BLE").ends_with(",,76,BLE"), "the BLE row keeps its own: {csv}");
}

#[test]
fn store_drops_without_blocking_when_queue_full() {
    // A stalled engine misses everything, including an assignment racing a node's
    // window. One lost observation is the cheaper failure, but must be visible.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.queue_depth = 1;
    config.batch_rows = 1024;
    config.batch_interval = Duration::from_secs(3600);
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    let flood: Vec<Record> =
        (0..2048).map(|n| observation(NODE, [n as u8; 6], -60, EPOCH_MS, Fix::none())).collect();
    let dropped = store.submit(flood);

    assert!(dropped > 0, "a depth-1 queue and no draining should overflow");
    assert_eq!(store.stats().dropped, dropped as u64, "and say so in the stats");
}

#[test]
fn store_reports_and_resets_peaks_when_taken() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store(&dir);
    assert_eq!(store.take_peaks(), StorePeaks::default(), "nothing queued or written yet");

    let flood: Vec<Record> =
        (0..512).map(|n| observation(NODE, [n as u8; 6], -60, EPOCH_MS, Fix::none())).collect();
    assert_eq!(store.submit(flood), 0);
    wait_for("every row to be written", || store.stats().written == 512);

    // The writer drains as the records go in, so how deep the queue got depends on the
    // machine; it held at least one record and never more than were sent.
    let peaks = store.take_peaks();
    assert!((1..=512).contains(&peaks.queue), "{peaks:?}");
    assert!(peaks.commit_us > 0, "a batch was written: {peaks:?}");
    assert_eq!(store.take_peaks(), StorePeaks::default(), "taking them starts again from 0");
    store.close();
}

#[test]
fn store_reports_batch_timings_on_close_when_asked() {
    // The benchmark divides rows by these, so a batch counted twice or not at all
    // would show up as a store that got better or worse for no reason.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_rows = 8;
    config.batch_interval = Duration::from_secs(3600);
    config.timings = true;
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    let rows: Vec<Record> = (0..20u8)
        .map(|n| observation(NODE, [0x02, 0, 0, 0, 0, n], -60, EPOCH_MS, Fix::none()))
        .collect();
    assert_eq!(store.submit(rows), 0);
    let report = store.close();

    assert_eq!(report.written, 20);
    assert_eq!(report.dropped, 0);
    // Two full batches of eight, then the four left over at close.
    let rows: Vec<usize> = report.batches.iter().map(|b| b.rows).collect();
    assert_eq!(rows, [8, 8, 4], "{report:?}");
    for batch in &report.batches {
        assert!(batch.commit <= batch.batch, "a commit is part of its batch: {report:?}");
    }
    assert!(
        report.batches.windows(2).all(|w| w[0].committed_at <= w[1].committed_at),
        "in the order they committed, so a warm-up can be cut off the front: {report:?}"
    );
}

#[test]
fn store_omits_batch_timings_when_not_asked() {
    // The list grows per commit for as long as a capture runs.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store(&dir);
    assert_eq!(store.submit(vec![observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none())]), 0);
    let report = store.close();
    assert_eq!(report.written, 1);
    assert!(report.batches.is_empty());
}

#[test]
fn store_copies_and_truncates_wal_when_checkpoint_background() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(5);
    // Any WAL at all is past the limit, so every pass that catches up is followed by a
    // truncation, and the last one, with the writer gone, always catches up.
    config.checkpoint = Checkpoint::Background { every: Duration::ZERO, truncate_at: 0 };
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    for n in 0..20u8 {
        let rows: Vec<Record> = (0..10u8)
            .map(|m| observation(NODE, [0x02, 0, 0, 0, n, m], -60, EPOCH_MS, Fix::none()))
            .collect();
        assert_eq!(store.submit(rows), 0);
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(50));
    let report = store.close();

    assert!(!report.checkpoints.passes.is_empty(), "{report:?}");
    assert!(!report.checkpoints.truncations.is_empty(), "{report:?}");
    let conn = open_readonly(&path).expect("reopening read-only");
    let rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).expect("counting");
    assert_eq!(rows, 200, "copying the WAL back from another connection loses nothing");
}

#[test]
fn store_counts_caught_up_passes_when_checkpoint_background() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_rows = 10;
    // The row count is the only thing allowed to decide a commit: the writer also
    // flushes once `batch_interval` has passed since the last one, so a drain that
    // outruns a short interval splits one submit into two commits, two wake-ups and two
    // passes — and then a pass covering the first half satisfies a wait meant for the
    // second. An interval no drain reaches leaves one submit worth exactly one commit,
    // which `timings` lets this test insist on rather than assume.
    config.batch_interval = Duration::from_secs(60);
    config.timings = true;
    config.checkpoint = Checkpoint::Background { every: Duration::ZERO, truncate_at: u64::MAX };
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    assert_eq!(store.checkpoints_caught_up(), 0, "nothing has been committed yet");
    for n in 0..3u64 {
        let caught_up = store.checkpoints_caught_up();
        let rows: Vec<Record> = (0..10u8)
            .map(|m| observation(NODE, [0x02, 0, 0, 2, n as u8, m], -60, EPOCH_MS, Fix::none()))
            .collect();
        assert_eq!(store.submit(rows), 0);
        // Sampled before the submit, not after it: one commit means one pass, and that
        // pass can finish before a sample taken later reads the count, leaving a wait for
        // a further increment that no other commit is coming to provide.
        wait_for("a checkpoint to catch up", || store.checkpoints_caught_up() > caught_up);
    }
    let report = store.close();
    assert_eq!(report.batches.len(), 3, "one commit per submit");
}

#[test]
fn store_counts_no_caught_up_passes_when_checkpoint_inline() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_rows = 10;
    config.batch_interval = Duration::from_secs(60);
    config.checkpoint = Checkpoint::Inline;
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    let rows: Vec<Record> = (0..10u8)
        .map(|m| observation(NODE, [0x02, 0, 0, 3, 0, m], -60, EPOCH_MS, Fix::none()))
        .collect();
    assert_eq!(store.submit(rows), 0);
    wait_for("the batch to commit", || store.stats().written >= 10);
    // There is no checkpointer thread to count, and the commits do their own.
    assert_eq!(store.checkpoints_caught_up(), 0);
    store.close();
}

/// The WAL file's size after twenty small commits, each settled before the next, under
/// `checkpoint`.
///
/// Settled means committed, and — where there is a checkpointer — checkpointed: what the
/// comparison below is about is what the WAL holds once a pass has had its turn, not how
/// much of one a given machine got through in a fixed wait.
fn wal_after_settled_commits(checkpoint: Checkpoint) -> u64 {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.batch_rows = 50;
    // The row count is the only thing allowed to decide a commit: the writer also
    // flushes once `batch_interval` has passed since the last one, so a drain that
    // outruns a short interval splits one submit into two commits, two wake-ups and two
    // passes — and then a pass covering the first half satisfies a wait meant for the
    // second. An interval no drain reaches leaves one submit worth exactly one commit,
    // which `timings` lets this test insist on rather than assume.
    config.batch_interval = Duration::from_secs(60);
    config.timings = true;
    // So that inline, SQLite's own checkpoint never rewinds the WAL either.
    config.wal_autocheckpoint_pages = Some(1_000_000);
    config.checkpoint = checkpoint;
    let checkpointed = matches!(checkpoint, Checkpoint::Background { .. });
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    for n in 0..20u64 {
        let caught_up = store.checkpoints_caught_up();
        let rows: Vec<Record> = (0..50u8)
            .map(|m| observation(NODE, [0x02, 0, 0, 1, n as u8, m], -60, EPOCH_MS, Fix::none()))
            .collect();
        assert_eq!(store.submit(rows), 0);
        wait_for("the batch to commit", || store.stats().written >= (n + 1) * 50);
        if checkpointed {
            wait_for("a checkpoint to catch up", || store.checkpoints_caught_up() > caught_up);
        }
    }
    let size = std::fs::metadata(dir.path().join("wartui.db-wal")).map_or(0, |m| m.len());
    let report = store.close();
    // What the waiting above rests on, checked rather than described: a submit the writer
    // split in two would put a commit's frames in the WAL that no pass was waited for.
    assert_eq!(report.batches.len(), 20, "one commit per submit");
    size
}

#[test]
fn store_keeps_wal_one_commit_long_when_checkpoint_background() {
    let inline = wal_after_settled_commits(Checkpoint::Inline);
    // Never truncated, so a small WAL can only be the writer rewinding it.
    let background = wal_after_settled_commits(Checkpoint::Background {
        every: Duration::ZERO,
        truncate_at: u64::MAX,
    });
    // Never checkpointed, the WAL holds all twenty commits — 87 frames of it. Rewound, the
    // file stops at its high-water mark of about 15, since rewinding reuses the frames
    // rather than shortening the file. Every pass has caught up before the next commit is
    // sent, so the sixfold gap that leaves is settled rather than raced for: the margin
    // here is slack, not the thing under test.
    assert!(background * 4 < inline, "background {background} bytes, inline {inline}");
}

#[test]
fn store_background_checkpoint_runs_owed_pass_when_fleet_quiet() {
    // The writer's own checkpoint is off, so a wake-up the checkpointer passes over would
    // leave that commit unsynced until the next commit, which a quiet fleet never sends.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_interval = Duration::from_millis(10);
    config.checkpoint =
        Checkpoint::Background { every: Duration::from_millis(200), truncate_at: u64::MAX };
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");

    // Two commits well inside one interval, then nothing.
    assert_eq!(store.submit(vec![observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none())]), 0);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(store.submit(vec![observation(NODE, [0xBB; 6], -60, EPOCH_MS, Fix::none())]), 0);
    std::thread::sleep(Duration::from_millis(500));

    let closing = std::time::Instant::now();
    let report = store.close();
    let before_close = report.checkpoints.passes.iter().filter(|p| p.finished_at < closing).count();
    assert!(
        before_close >= 2,
        "the second commit's pass runs when the interval is up, not at close: {report:?}"
    );
}

#[test]
fn store_defaults_to_background_checkpoint_when_config_new() {
    // What the card measured best: see `StoreConfig::new`.
    let config = StoreConfig::new("unused.db");
    assert_eq!(
        config.checkpoint,
        Checkpoint::Background { every: Duration::ZERO, truncate_at: 64 << 20 }
    );
    assert_eq!(
        (config.batch_interval, config.batch_rows, config.queue_depth),
        (Duration::from_secs(1), 16_384, 16_384)
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let store = store(&dir);
    assert_eq!(store.submit(vec![observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none())]), 0);
    let report = store.close();
    assert!(!report.checkpoints.passes.is_empty(), "{report:?}");
}

#[test]
fn store_reports_no_passes_on_close_when_checkpoint_inline() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_interval = Duration::from_millis(10);
    config.checkpoint = Checkpoint::Inline;
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");
    assert_eq!(store.submit(vec![observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none())]), 0);
    let report = store.close();
    assert!(report.checkpoints.passes.is_empty() && report.checkpoints.truncations.is_empty());
}

fn has_bssid_index(path: &std::path::Path) -> bool {
    open_readonly(path)
        .expect("reopening read-only")
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'obs_bssid'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("asking for the index")
        == 1
}

#[test]
fn store_exports_every_network_when_bssid_unindexed() {
    // The index cost the card fourteen times the writes and made export slower; the
    // export's scan and sort must still find every network without it.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -70, EPOCH_MS, fixed(37.0, -122.0)),
            observation(OTHER, [0xAA; 6], -50, EPOCH_MS + 1_000, fixed(37.1, -122.1)),
            observation(NODE, [0xBA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );
    assert!(!has_bssid_index(&dir.path().join("wartui.db")));

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 2, "{csv}");
    assert!(csv.contains(",-50,37.1,"), "still the strongest sighting: {csv}");
}

#[test]
fn store_uses_page_size_when_creating_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.page_size = Some(16_384);
    config.cache_kib = Some(32 * 1024);
    config.wal_autocheckpoint_pages = Some(4000);
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    Store::create(&config, &info, EPOCH_MS).expect("creating the store").close();

    let conn = open_readonly(&path).expect("reopening read-only");
    let page_size: i64 =
        conn.pragma_query_value(None, "page_size", |r| r.get(0)).expect("reading page_size");
    assert_eq!(page_size, 16_384, "set before WAL wrote the header, or it would be 4096");
}

#[test]
fn wigle_csv_takes_first_seen_from_unpositioned_when_window_zero() {
    // A capture without --lat then a positioned one hours later is a normal way to
    // end up with both in one file. Pinned to the zero window, which folds a
    // network's whole capture into one row; the default window splits these
    // three hours apart and is covered below.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none()),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 10_800_000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) =
        export_with(&conn, ExportFilter { recapture_secs: 0, ..ExportFilter::default() });
    assert_eq!(summary.rows, 1);
    assert!(
        csv.lines().nth(2).expect("a row").contains("2026-05-01 13:34:37"),
        "the earliest sighting's time, not the earliest positioned one: {csv}"
    );
}

#[test]
fn wigle_csv_takes_first_seen_from_window_start_when_unpositioned() {
    // The default window splits these three hours apart, so the second window's
    // row says when *it* opened, and the first window — heard, never positioned —
    // is counted rather than written.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none()),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 10_800_000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 1);
    assert_eq!(summary.unpositioned, 1, "the window with no fix is counted, not written");
    assert!(
        csv.lines().nth(2).expect("a row").contains("2026-05-01 16:34:37"),
        "the second window's first sighting, not the capture's: {csv}"
    );
}

#[test]
fn wigle_csv_opens_new_row_when_sighting_past_recapture_window() {
    // WDGWars skips a re-scan of the same AP within the hour from scoring, so
    // a sighting just past the hour after the window opened has to be a row
    // of its own — the first re-hearing the site will count.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -55, EPOCH_MS + 3_601_000, fixed(37.1, -122.1)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 2, "an hour and a second apart is two captures");
    let rows: Vec<&str> = csv.lines().skip(2).collect();
    assert!(rows[0].contains("2026-05-01 13:34:37"), "{}", rows[0]);
    assert!(rows[1].contains("2026-05-01 14:34:38"), "{}", rows[1]);
    assert!(rows[1].contains("37.1,-122.1"), "the re-hearing's position: {}", rows[1]);
}

#[test]
fn wigle_csv_keeps_sighting_in_window_when_exactly_one_hour_later() {
    // The site's cooldown is one hour per user and MAC — "re-scanning the
    // same AP within 1h is silently skipped from scoring; GPS may still be
    // refined" — and the window is that rule, inclusive at the boundary,
    // because a sighting opens the next window only past the width. So a
    // re-hearing exactly on the hour stays in the row already submitted, and
    // the stronger reading of the two is the one it carries: the GPS
    // refinement the rule allows.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -50, EPOCH_MS + 3_600_000, fixed(37.5, -122.5)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 1, "an hour apart on the mark is one capture");
    let row = csv.lines().nth(2).expect("a row");
    assert!(row.contains("37.5,-122.5"), "the strongest of the two: {row}");
    assert!(row.contains("2026-05-01 13:34:37"), "and the window's own start: {row}");
}

#[test]
fn wigle_csv_orders_rows_by_window_start_when_stored_out_of_order() {
    // The fold visits networks in address order; the file must not.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(OTHER, [0xBA; 6], -60, EPOCH_MS + 60_000, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );

    let (csv, _) = export(&conn);
    let rows: Vec<&str> = csv.lines().skip(2).collect();
    assert!(rows[0].starts_with("AA:AA:AA:AA:AA:AA,"), "{}", rows[0]);
    assert!(rows[1].starts_with("BA:BA:BA:BA:BA:BA,"), "{}", rows[1]);
}

#[test]
fn wigle_csv_interleaves_networks_by_window_start_when_windows_alternate() {
    // The fold finishes AA's windows before it reaches BA, so an order by network
    // would write AA, AA, BA. The file is ordered by window start.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xBA; 6], -60, EPOCH_MS + 1_800_000, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 7_200_000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 3, "{csv}");
    let macs: Vec<&str> = csv.lines().skip(2).map(|row| &row[..17]).collect();
    assert_eq!(macs, ["AA:AA:AA:AA:AA:AA", "BA:BA:BA:BA:BA:BA", "AA:AA:AA:AA:AA:AA"]);
}

#[test]
fn wigle_csv_writes_identical_file_when_run_twice_on_one_connection() {
    // The sort goes through a temporary table on the caller's connection, which a
    // second export must start afresh rather than add to.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xBA; 6], -60, EPOCH_MS + 60_000, fixed(37.0, -122.0)),
        ],
    );

    let first = export(&conn);
    let second = export(&conn);
    assert_eq!(first.1.rows, 2);
    assert_eq!(first, second);
}

#[test]
fn store_refuses_database_when_schema_version_differs() {
    // Reading another build's tables as this build's would export rows of the wrong
    // shape, or values that meant something else.
    //
    // 1 is deliberately not in this list: it is the marker this build writes
    for found in [99, 2] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wartui.db");
        let other = Connection::open(&path).expect("creating");
        other.pragma_update(None, "user_version", found).expect("stamping");
        drop(other);

        let opened = open_readonly(&path);
        assert!(
            matches!(
                opened,
                Err(StoreError::SchemaMismatch { found: f, ours })
                    if f == found && ours == SCHEMA_VERSION
            ),
            "expected v{found} to be refused, got {opened:?}"
        );

        let still: i32 = Connection::open(&path)
            .expect("reopening")
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("reading the version");
        assert_eq!(still, found, "and the marker must be left alone");
    }
}

/// Create a store at `path` and close it, leaving a stamped file behind.
fn create(path: &std::path::Path) {
    open_at(path).close();
}

#[test]
fn store_reopens_database_when_fingerprint_matches() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    create(&path);
    open_readonly(&path).expect("reopening a file this build wrote");
}

#[test]
fn store_refuses_database_when_fingerprint_differs() {
    // The marker stays at 1 while the schema changes shape, so the fingerprint is what
    // tells this build's file from another's.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    create(&path);
    Connection::open(&path)
        .expect("reopening")
        .execute("UPDATE kv SET v = '00000000deadbeef' WHERE k = 'schema_fingerprint'", [])
        .expect("overwriting the fingerprint");

    let opened = open_readonly(&path);
    assert!(
        matches!(opened, Err(StoreError::SchemaDiffers { found: Some(0xdead_beef), ours })
            if ours != 0xdead_beef),
        "expected the other fingerprint to be refused, got {opened:?}"
    );
}

#[test]
fn store_refuses_database_when_fingerprint_missing() {
    // A file from a build that stamped no fingerprint carries this build's marker over
    // whatever shape that build's tables had.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    create(&path);
    Connection::open(&path)
        .expect("reopening")
        .execute("DELETE FROM kv WHERE k = 'schema_fingerprint'", [])
        .expect("deleting the fingerprint");

    let opened = open_readonly(&path);
    assert!(
        matches!(opened, Err(StoreError::SchemaDiffers { found: None, .. })),
        "expected a file without a fingerprint to be refused, got {opened:?}"
    );
}

#[test]
fn store_refuses_create_when_file_exists() {
    // A capture holds one run, so a second run into the same path is refused rather
    // than appended to, and the file it found is left exactly as it was.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    create(&path);
    let before = std::fs::read(&path).expect("reading the capture");

    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let created = Store::create(&StoreConfig::new(&path), &info, EPOCH_MS);
    assert!(
        matches!(&created, Err(StoreError::Exists(p)) if *p == path),
        "expected the existing file to be refused, got {created:?}"
    );
    drop(created);
    assert_eq!(std::fs::read(&path).expect("rereading the capture"), before);
}

#[test]
fn store_stamps_schema_version_when_creating_file() {
    // The stamp is what makes the next read-only open recognize the file as this
    // build's.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    open_at(&path).close();

    let conn = open_readonly(&path).expect("reopening");
    let version: i32 =
        conn.pragma_query_value(None, "user_version", |row| row.get(0)).expect("the version");
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn store_records_bridge_when_bridge_announces() {
    // A capture says which dongle produced it.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![Record::Bridge(BridgeSeen {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
            chip: "Esp32C6".to_owned(),
            fw_version: "0.1.0".to_owned(),
        })],
    );

    let (mac, chip, fw): (Vec<u8>, String, String) = conn
        .query_row("SELECT bridge_mac, bridge_chip, bridge_fw FROM capture", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .expect("the capture row");
    assert_eq!(mac, vec![0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24]);
    assert_eq!((chip.as_str(), fw.as_str()), ("Esp32C6", "0.1.0"));
}

// ---------------------------------------------------------------------------
// Phase 4: assignment
// ---------------------------------------------------------------------------

fn assignment(counter: u64, outcome: AdminOutcome, latency_us: Option<u32>) -> Record {
    Record::Assignment(AssignmentSent {
        node_mac: NODE,
        counter,
        wire_version: wartui_proto::air::wire_epoch(counter),
        channels: wartui_proto::plan::ChannelSet::from_run(wartui_proto::plan::IndexRun::new(5, 5)),
        ble: false,
        created_at_ms: EPOCH_MS,
        delivered_at_ms: Some(EPOCH_MS + 5),
        outcome,
        latency_us,
    })
}

#[test]
fn store_records_every_assignment_when_outcomes_differ() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let store = open_at(&path);
    // Every attempt gets a row, which is what makes "the fleet keeps drifting off its
    // channels" a query rather than a story.
    store.submit(vec![
        assignment(1, AdminOutcome::Unacked, None),
        assignment(1, AdminOutcome::Acked, Some(4_500)),
    ]);
    store.close();

    let conn = open_readonly(&path).expect("reopening");
    let rows: Vec<(String, Option<u32>, u8)> = conn
        .prepare("SELECT outcome, latency_us, wire_version FROM assignment ORDER BY id")
        .expect("preparing")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("querying")
        .map(|r| r.expect("row"))
        .collect();

    assert_eq!(
        rows,
        vec![("unacked".to_owned(), None, 1), ("acked".to_owned(), Some(4_500), 1)],
        "a retry is a second row, not an update: the table is append-only"
    );
}

#[test]
fn wigle_csv_counts_network_once_when_heard_repeatedly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let at = |n: i64| EPOCH_MS + n * 1000;
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, at(0), fixed(37.0, -122.0)),
            observation(NODE, [0xBA; 6], -60, at(1), fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, at(2), fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, at(3), fixed(37.0, -122.0)),
        ],
    );

    let (_, summary) = export(&conn);
    assert_eq!(summary.wifi.networks, 2);
    assert_eq!(summary.wifi.sightings, 4);
    assert_eq!(summary.wifi.rows, 2);
    assert_eq!((summary.first_rx, summary.last_rx), (Some(at(0)), Some(at(3))));
}

#[test]
fn wigle_csv_skips_invalid_wifi_addresses_when_capture_preserves_them() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0x02; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            ble_observation(OTHER, [0xC3; 6], EPOCH_MS + 1000, fixed(37.0, -122.0)),
            observation(NODE, [0; 6], -20, EPOCH_MS - 1000, fixed(37.0, -122.0)),
            observation(NODE, [0xFF; 6], -20, EPOCH_MS + 3000, fixed(37.0, -122.0)),
            observation(OTHER, [0x01; 6], -20, EPOCH_MS + 4000, Fix::none()),
            observation(NODE, [0x01; 6], -20, EPOCH_MS + 5000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(macs(&csv), ["02:02:02:02:02:02", "C3:C3:C3:C3:C3:C3"], "{csv}");
    assert_eq!(summary.rows, 2);
    assert_eq!((summary.wifi.networks, summary.wifi.sightings), (1, 1));
    assert_eq!((summary.ble.networks, summary.ble.sightings), (1, 1));
    assert_eq!(summary.positions, wartui_core::export::Positions { gps: 0, fixed: 2, none: 0 });
    assert_eq!(summary.unpositioned, 0);
    assert_eq!(summary.unknown_kind, 0);
    assert_eq!(summary.invalid_address, 4);
    assert_eq!(summary.wifi_channel_zero, 0);
    assert_eq!(
        summary.nodes.iter().map(|n| (n.mac, n.wifi, n.ble)).collect::<Vec<_>>(),
        [(NODE, 1, 0), (OTHER, 0, 1)]
    );
    assert_eq!((summary.first_rx, summary.last_rx), (Some(EPOCH_MS), Some(EPOCH_MS + 1000)));
    assert_eq!(summary.last_id, Some(6));
    let stored: i64 = conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, 6, "export does not remove evidence from the store");

    let rw = open_readwrite(&dir.path().join("wartui.db")).expect("opening read-write");
    record_upload(&rw, summary.last_id.unwrap(), EPOCH_MS + 6000, 7, summary.rows)
        .expect("recording the cutoff locally");
    let (csv, summary) = export_with(&conn, AFTER_UPLOADS);
    assert!(macs(&csv).is_empty());
    assert_eq!(summary.last_id, None);
    assert_eq!((summary.invalid_address, summary.wifi_channel_zero), (0, 0));
}

#[test]
fn wigle_csv_preserves_channels_when_wifi_is_zero_or_off_pool() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            on_channel([0x02; 6], 0, EPOCH_MS),
            on_channel([0x04; 6], 13, EPOCH_MS + 1000),
            on_channel([0x06; 6], 14, EPOCH_MS + 2000),
            on_channel([0x08; 6], 169, EPOCH_MS + 3000),
            ble_observation(NODE, [0xC3; 6], EPOCH_MS + 4000, fixed(37.0, -122.0)),
            on_channel([0; 6], 0, EPOCH_MS + 5000),
            on_channel([0x02; 6], 0, EPOCH_MS + 6000),
        ],
    );

    let (csv, summary) = export(&conn);
    let channels: Vec<_> =
        csv.lines().skip(2).map(|line| line.split(',').nth(4).unwrap()).collect();
    assert_eq!(channels, ["0", "13", "14", "169", "0"], "{csv}");
    assert_eq!(summary.rows, 5);
    assert_eq!(summary.wifi.sightings, 5);
    assert_eq!(summary.invalid_address, 1);
    assert_eq!(summary.wifi_channel_zero, 3);
    assert_eq!(summary.last_id, Some(7));
    assert_eq!(summary.wifi_bands, wartui_core::export::Bands { ghz2_4: 2, ghz5: 1, other: 1 });
}

#[test]
fn wigle_csv_counts_only_quality_and_cutoff_when_all_wifi_addresses_invalid() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            on_channel([0; 6], 0, EPOCH_MS),
            observation(NODE, [0xFF; 6], -60, EPOCH_MS + 1000, Fix::none()),
            on_channel([0x01; 6], 0, EPOCH_MS + 2000),
        ],
    );

    let (csv, summary) = export(&conn);
    assert!(macs(&csv).is_empty(), "{csv}");
    assert_eq!(
        summary,
        wartui_core::export::ExportSummary {
            invalid_address: 3,
            wifi_channel_zero: 2,
            last_id: Some(3),
            ..wartui_core::export::ExportSummary::default()
        }
    );
}

#[test]
fn wigle_csv_counts_wifi_and_ble_apart_when_capture_holds_both() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            ble_observation(NODE, [0xCC; 6], EPOCH_MS, fixed(37.0, -122.0)),
            ble_observation(NODE, [0xCC; 6], EPOCH_MS + 1000, fixed(37.0, -122.0)),
            // One address heard as both kinds is a network of each.
            ble_observation(NODE, [0xAA; 6], EPOCH_MS + 2000, fixed(37.0, -122.0)),
        ],
    );

    let (_, summary) = export(&conn);
    assert_eq!(summary.wifi, wartui_core::export::KindStats { networks: 1, sightings: 1, rows: 1 });
    // AA's Bluetooth sighting is a network and a row of its own, beside its Wi-Fi one.
    assert_eq!(summary.ble, wartui_core::export::KindStats { networks: 2, sightings: 3, rows: 2 });
}

#[test]
fn wigle_csv_skips_sighting_when_kind_is_neither_wifi_nor_ble() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            ble_observation(NODE, [0xCC; 6], EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );
    let (before_csv, before) = export(&conn);

    // No build writes this kind, so only an edited file holds one.
    let rw = Connection::open(dir.path().join("wartui.db")).expect("opening for writing");
    rw.execute(
        "INSERT INTO observation (node_mac, rx_at, link_rssi, bssid, ssid, security, channel, rssi,
                                  kind, rcoi, mfgr_id, lat, lon, alt, accuracy, pos_source, pos_at,
                                  raw_body)
         SELECT node_mac, rx_at + 500, link_rssi, X'DDDDDDDDDDDD', ssid, security, channel, rssi,
                'zigbee', rcoi, mfgr_id, lat, lon, alt, accuracy, pos_source, pos_at, raw_body
         FROM observation WHERE bssid = X'AAAAAAAAAAAA'",
        [],
    )
    .expect("inserting the foreign row");
    let zigbee_id = rw.last_insert_rowid();

    let (csv, summary) = export(&conn);
    assert!(!csv.contains("DD:DD:DD:DD:DD:DD"), "{csv}");
    assert_eq!(csv, before_csv, "the other rows are unchanged");
    assert_eq!(summary.unknown_kind, 1);
    assert_eq!((summary.wifi, summary.ble), (before.wifi, before.ble));
    assert_eq!(summary.positions, before.positions);
    // The skipped row is still walked, so an upload's cutoff covers it.
    assert_eq!(summary.last_id, Some(zigbee_id));
}

#[test]
fn wigle_csv_counts_sightings_per_node_when_two_nodes_report() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(OTHER, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 1000, fixed(37.0, -122.0)),
            observation(NODE, [0xBA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            ble_observation(OTHER, [0xCC; 6], EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );

    let (_, summary) = export(&conn);
    let nodes: Vec<_> = summary.nodes.iter().map(|n| (n.mac.as_slice(), n.wifi, n.ble)).collect();
    // Sorted by address, whatever order the sightings came in.
    assert_eq!(nodes, [(&NODE[..], 2, 0), (&OTHER[..], 1, 1)]);
}

#[test]
fn wigle_csv_counts_network_once_per_band_when_heard_on_both() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            on_channel([0xAA; 6], 6, EPOCH_MS),
            on_channel([0xAA; 6], 36, EPOCH_MS + 1000),
            on_channel([0xAA; 6], 6, EPOCH_MS + 2000),
            on_channel([0xBA; 6], 149, EPOCH_MS),
            on_channel([0xBA; 6], 149, EPOCH_MS + 1000),
        ],
    );

    let (_, summary) = export(&conn);
    assert_eq!(summary.wifi.networks, 2);
    assert_eq!(summary.wifi_bands, wartui_core::export::Bands { ghz2_4: 1, ghz5: 2, other: 0 });
}

#[test]
fn wigle_csv_tallies_position_sources_when_fix_source_varies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let gps = Fix { source: PositionSource::Gps, ..fixed(37.0, -122.0) };
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, gps),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 1000, gps),
            observation(NODE, [0xBA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xCC; 6], -60, EPOCH_MS, Fix::none()),
        ],
    );

    let (_, summary) = export(&conn);
    assert_eq!(summary.positions, wartui_core::export::Positions { gps: 2, fixed: 1, none: 1 });
}

/// Three networks heard a minute apart, stored as ids 1, 2 and 3, so a cutoff can fall
/// between any two of them.
fn three_sightings() -> Vec<Record> {
    vec![
        observation(NODE, [0xA0; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
        observation(NODE, [0xA2; 6], -60, EPOCH_MS + 60_000, fixed(37.0, -122.0)),
        observation(NODE, [0xA4; 6], -60, EPOCH_MS + 120_000, fixed(37.0, -122.0)),
    ]
}

/// The MACs a CSV's rows name, in order.
fn macs(csv: &str) -> Vec<&str> {
    csv.lines().skip(2).filter_map(|line| line.split(',').next()).collect()
}

const AFTER_UPLOADS: ExportFilter = ExportFilter {
    recapture_secs: wartui_core::export::DEFAULT_RECAPTURE_SECS,
    after_uploads: true,
};

#[test]
fn wigle_csv_skips_sightings_through_cutoff_when_after_uploads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(&dir, three_sightings());
    let rw = open_readwrite(&dir.path().join("wartui.db")).expect("opening read-write");
    // The cutoff is the second sighting itself: it was sent, so it is not sent again.
    let id = record_upload(&rw, 2, EPOCH_MS + 90_000, 7, 2).expect("recording");
    set_upload_result(&rw, id, "done").expect("setting the result");

    let (csv, summary) = export_with(&conn, AFTER_UPLOADS);
    assert_eq!(macs(&csv), ["A4:A4:A4:A4:A4:A4"], "{csv}");
    assert_eq!(summary.last_id, Some(3));
}

#[test]
fn wigle_csv_ignores_upload_for_cutoff_when_it_failed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(&dir, three_sightings());
    let rw = open_readwrite(&dir.path().join("wartui.db")).expect("opening read-write");
    record_upload(&rw, 1, EPOCH_MS + 30_000, 7, 1).expect("recording");
    // A later job the site reported failed imported nothing, so the cutoff stays at job 7.
    let failed = record_upload(&rw, 2, EPOCH_MS + 90_000, 8, 1).expect("recording");
    set_upload_result(&rw, failed, "failed").expect("setting the result");

    let (csv, summary) = export_with(&conn, AFTER_UPLOADS);
    assert_eq!(macs(&csv), ["A2:A2:A2:A2:A2:A2", "A4:A4:A4:A4:A4:A4"], "{csv}");
    assert_eq!(summary.rows, 2);
}

#[test]
fn wigle_csv_ignores_uploads_when_after_uploads_off() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(&dir, three_sightings());
    let rw = open_readwrite(&dir.path().join("wartui.db")).expect("opening read-write");
    record_upload(&rw, 3, EPOCH_MS + 150_000, 7, 3).expect("recording");

    let (_, summary) = export(&conn);
    assert_eq!(summary.rows, 3);
}

#[test]
fn wigle_csv_includes_row_when_stored_after_upload_at_cutoff_time() {
    // One frame's sightings share an rx_at, and the store can commit them in two batches.
    // An upload that read between the commits covered only the first, so the second still
    // goes on the next upload.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xA0; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xA2; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );
    let rw = open_readwrite(&dir.path().join("wartui.db")).expect("opening read-write");
    record_upload(&rw, 1, EPOCH_MS + 30_000, 7, 1).expect("recording");

    let (csv, _) = export_with(&conn, AFTER_UPLOADS);
    assert_eq!(macs(&csv), ["A2:A2:A2:A2:A2:A2"], "{csv}");
}
