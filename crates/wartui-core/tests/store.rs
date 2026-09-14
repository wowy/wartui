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
use wartui_core::record::{
    AdminOutcome, AssignmentSent, BridgeSeen, Heartbeat, NodeSeen, Observation, Record,
};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;
use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun};

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
        raw_body: b"raw".to_vec(),
    })
}

/// A store on a fresh temporary database, plus the directory keeping it alive.
fn store(dir: &tempfile::TempDir) -> Store {
    open_at(&dir.path().join("wartui.db"))
}

/// A store on one named path, so a test can reopen the same database.
fn open_at(path: &std::path::Path) -> Store {
    let mut config = StoreConfig::new(path);
    // Small and quick, so a test does not sit waiting for a batch window.
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(10);
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
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
            Record::Node(NodeSeen {
                mac: NODE,
                first_seen_ms: EPOCH_MS,
                last_seen_ms: EPOCH_MS,
                capabilities: Some("wartui/0.1;ble,5g".to_owned()),
            }),
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
            Record::Node(NodeSeen {
                mac: NODE,
                first_seen_ms: EPOCH_MS,
                last_seen_ms: EPOCH_MS,
                capabilities: Some("wartui/0.1;ble,5g".to_owned()),
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
    assert_eq!(caps.as_deref(), Some("wartui/0.1;ble,5g"), "what it last said it was, kept");
}

#[test]
fn every_sighting_is_kept_rather_than_deduplicated_on_the_way_in() {
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
fn the_export_picks_the_strongest_sighting_but_the_earliest_first_seen() {
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

    assert_eq!(summary.networks, 1);
    assert!(row.contains("38.5,-123.5"), "the strongest sighting's position: {row}");
    assert!(row.contains(",-50,"), "and its RSSI: {row}");
    assert!(row.starts_with("AA:AA:AA:AA:AA:AA,"));
    assert!(row.contains("2026-05-01 13:34:37"), "the earliest sighting's time: {row}");
}

#[test]
fn the_header_is_the_wigle_v1_6_pair() {
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
fn a_network_nobody_had_a_position_for_is_counted_rather_than_written() {
    // Not an error and not silent: the operator needs to know how much of a capture
    // is waiting on a `--lat`/`--lon` before uploading.
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
fn a_cloaked_ssid_recorded_before_the_parser_trimmed_it_still_exports_clean() {
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
    assert!(row.ends_with(",,,BLE"), "{row}");
    assert!(row.contains("AA:AA:AA:AA:AA:AA,,[BLE],"), "an empty SSID stays empty: {row}");
    // Channel 0 with a blank frequency behind it: a passive scan has no
    // "device type" code to put there, and the row says so by leaving it out.
    assert!(row.contains(",0,,-60,"), "channel 0, then no frequency: {row}");
}

#[test]
fn a_full_queue_drops_and_counts_rather_than_blocking_the_engine() {
    // A stalled engine misses everything, including an assignment racing a node's
    // window. One lost observation is the cheaper failure, but must be visible.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.queue_depth = 1;
    config.batch_rows = 1024;
    config.batch_interval = Duration::from_secs(3600);
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

    let flood: Vec<Record> =
        (0..2048).map(|n| observation(NODE, [n as u8; 6], -60, EPOCH_MS, Fix::none())).collect();
    let dropped = store.submit(flood);

    assert!(dropped > 0, "a depth-1 queue and no draining should overflow");
    assert_eq!(store.stats().dropped, dropped as u64, "and say so in the stats");
}

#[test]
fn closing_reports_every_committed_batch_when_timings_were_asked_for() {
    // The benchmark divides rows by these, so a batch counted twice or not at all
    // would show up as a store that got better or worse for no reason.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_rows = 8;
    config.batch_interval = Duration::from_secs(3600);
    config.timings = true;
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

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
fn a_capture_keeps_no_timings_unless_asked() {
    // The list grows per commit for as long as a capture runs.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store(&dir);
    assert_eq!(store.submit(vec![observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none())]), 0);
    let report = store.close();
    assert_eq!(report.written, 1);
    assert!(report.batches.is_empty());
}

#[test]
fn a_page_size_asked_for_is_the_one_a_new_database_gets() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.page_size = Some(16_384);
    config.cache_kib = Some(32 * 1024);
    config.wal_autocheckpoint_pages = Some(4000);
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    Store::open(&config, &session, EPOCH_MS).expect("opening the store").close();

    let conn = open_readonly(&path).expect("reopening read-only");
    let page_size: i64 =
        conn.pragma_query_value(None, "page_size", |r| r.get(0)).expect("reading page_size");
    assert_eq!(page_size, 16_384, "set before WAL wrote the header, or it would be 4096");
}

#[test]
fn first_seen_comes_from_the_earliest_sighting_even_if_it_had_no_position() {
    // A capture without --lat then a positioned one hours later is a normal way to
    // end up with both in one file.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none()),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 10_800_000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.networks, 1);
    assert!(
        csv.lines().nth(2).expect("a row").contains("2026-05-01 13:34:37"),
        "the earliest sighting's time, not the earliest positioned one: {csv}"
    );
}

#[test]
fn a_database_from_a_newer_wartui_is_refused_rather_than_written_into() {
    // `CREATE TABLE IF NOT EXISTS` no-ops against a newer file's tables instead of
    // failing, so without this an older build appends rows of the wrong shape and
    // stamps the marker back down, leaving neither
    // build able to tell it had happened.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let future = Connection::open(&path).expect("creating");
    future.pragma_update(None, "user_version", 99).expect("stamping");
    drop(future);

    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let opened = Store::open(&StoreConfig::new(&path), &session, EPOCH_MS);
    assert!(
        matches!(opened, Err(wartui_core::store::StoreError::SchemaTooNew { found: 99, .. })),
        "expected a refusal, got {opened:?}"
    );

    let still: i32 = Connection::open(&path)
        .expect("reopening")
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("reading the version");
    assert_eq!(still, 99, "and the marker must be left alone");
}

#[test]
fn the_bridge_that_produced_a_capture_is_recorded_against_the_session() {
    // A file with several sessions from two different dongles has to be able to
    // say which produced which.
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
        .query_row("SELECT bridge_mac, bridge_chip, bridge_fw FROM session", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .expect("the session row");
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
        node_index: 0,
        node_count: 2,
        channels: wartui_proto::plan::ChannelSet::from_run(wartui_proto::plan::IndexRun::new(5, 5)),
        ble: false,
        created_at_ms: EPOCH_MS,
        delivered_at_ms: Some(EPOCH_MS + 5),
        outcome,
        latency_us,
    })
}

#[test]
fn every_assignment_attempt_gets_a_row_whether_or_not_it_landed() {
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
fn the_assignment_epoch_is_moved_forward_before_anything_can_be_sent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // Persisting the counter only after an
    // assignment goes out would let a crash in between hand the next run an
    // epoch a node already holds.
    let first = open_at(&path);
    assert_eq!(first.assignment_base(), 0, "a fresh database starts from nothing");
    first.close();

    let second = open_at(&path);
    assert!(
        second.assignment_base() >= 64,
        "opening books a block of epochs, so a crash can only skip them"
    );
    let base = second.assignment_base();
    second.submit(vec![assignment(base + 1, AdminOutcome::Acked, Some(1_000))]);
    second.close();

    let third = open_at(&path);
    assert!(
        third.assignment_base() > base + 1,
        "and spending one moves the reservation along with it"
    );
}

#[test]
fn a_database_from_the_previous_wartui_is_brought_forward_rather_than_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // v1's `assignment` table has no `outcome`, `latency_us` or `counter`, and
    // `CREATE TABLE IF NOT EXISTS` will not widen a table that already exists.
    let old = Connection::open(&path).expect("creating");
    old.execute_batch(
        "CREATE TABLE assignment (
           id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL, node_mac BLOB NOT NULL,
           wire_version INTEGER NOT NULL, node_index INTEGER NOT NULL,
           node_count INTEGER NOT NULL, start_idx INTEGER NOT NULL, end_idx INTEGER NOT NULL,
           created_at INTEGER NOT NULL, delivered_at INTEGER)",
    )
    .expect("v1 table");
    old.pragma_update(None, "user_version", 1).expect("stamping");
    drop(old);

    let store = open_at(&path);
    store.submit(vec![assignment(1, AdminOutcome::Acked, Some(2_000))]);
    store.close();

    let conn = open_readonly(&path).expect("reopening");
    let outcome: String = conn
        .query_row("SELECT outcome FROM assignment", [], |row| row.get(0))
        .expect("the row the migrated table can hold");
    assert_eq!(outcome, "acked");
}

#[test]
fn a_v2_assignment_row_keeps_its_channels_when_they_become_a_mask() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // v2 stored a contiguous run as a pair of bounds, which a mask expresses exactly
    // — and unlike v1's table these are real rows, since a v2 build could transmit.
    let old = Connection::open(&path).expect("creating");
    old.execute_batch(
        "CREATE TABLE session (
           id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER,
           bridge_mac BLOB, bridge_chip TEXT, bridge_fw TEXT,
           espnow_channel INTEGER NOT NULL, channel_pool TEXT NOT NULL, notes TEXT);
         INSERT INTO session (id, started_at, espnow_channel, channel_pool)
           VALUES (1, 0, 6, 'us');
         CREATE TABLE node (
           mac BLOB PRIMARY KEY, label TEXT, first_seen INTEGER NOT NULL,
           last_seen INTEGER NOT NULL, pinned_start_idx INTEGER, pinned_end_idx INTEGER);
         CREATE TABLE assignment (
           id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL, node_mac BLOB NOT NULL,
           counter INTEGER NOT NULL, wire_version INTEGER NOT NULL,
           node_index INTEGER NOT NULL, node_count INTEGER NOT NULL,
           start_idx INTEGER NOT NULL, end_idx INTEGER NOT NULL,
           created_at INTEGER NOT NULL, delivered_at INTEGER, outcome TEXT, latency_us INTEGER);
         INSERT INTO assignment
           (session_id, node_mac, counter, wire_version, node_index, node_count,
            start_idx, end_idx, created_at, delivered_at, outcome, latency_us)
         VALUES (1, x'0200005E1057', 4, 4, 0, 2, 14, 36, 1, 2, 'acked', 4500),
                (1, x'0200005E1057', 5, 5, 1, 2, 0, 0, 3, 4, 'unacked', NULL);",
    )
    .expect("v2 tables");
    old.pragma_update(None, "user_version", 2).expect("stamping");
    drop(old);

    let store = open_at(&path);
    store.close();

    let conn = open_readonly(&path).expect("reopening");
    let rows: Vec<(i64, bool, String)> = conn
        .prepare("SELECT channels, ble, outcome FROM assignment ORDER BY id")
        .expect("preparing")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("querying")
        .map(|r| r.expect("row"))
        .collect();

    let expect = |start, end| {
        i64::try_from(ChannelSet::from_run(IndexRun::new(start, end)).bits())
            .expect("a 40-bit mask")
    };
    assert_eq!(
        rows,
        vec![
            (expect(14, 36), false, "acked".to_owned()),
            (expect(0, 0), false, "unacked".to_owned()),
        ],
        "the bounds became the mask that says the same thing, and no v2 row could have had BLE"
    );

    // Nothing ever wrote these, in any version, so there is nothing to preserve.
    let pinned: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('node') WHERE name LIKE 'pinned%'")
        .expect("preparing")
        .query_map([], |row| row.get(0))
        .expect("querying")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(pinned, vec!["pinned_channels".to_owned()]);
}

#[test]
fn a_migration_that_fails_part_way_leaves_the_file_exactly_as_it_was() {
    // The v2 rebuild renames the old table before writing the new one, so a failure
    // committed statement by statement leaves a file that is neither shape and still
    // stamped v2 — a capture that can never be opened again, which is worse than one
    // that cannot be migrated today.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // A v2 file whose `counter` is nullable, holding one row that is null
    // there. v3 declares that column NOT NULL, so the rebuild fails on its
    // INSERT — after the rename has already happened.
    let old = Connection::open(&path).expect("creating");
    old.execute_batch(
        "CREATE TABLE assignment (
           id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL, node_mac BLOB NOT NULL,
           counter INTEGER, wire_version INTEGER NOT NULL,
           node_index INTEGER NOT NULL, node_count INTEGER NOT NULL,
           start_idx INTEGER NOT NULL, end_idx INTEGER NOT NULL,
           created_at INTEGER NOT NULL, delivered_at INTEGER, outcome TEXT, latency_us INTEGER);
         INSERT INTO assignment
           (session_id, node_mac, counter, wire_version, node_index, node_count,
            start_idx, end_idx, created_at)
         VALUES (1, x'0200005E1057', NULL, 4, 0, 2, 14, 36, 1);",
    )
    .expect("v2 tables");
    old.pragma_update(None, "user_version", 2).expect("stamping");
    drop(old);

    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let opened = Store::open(&StoreConfig::new(&path), &session, EPOCH_MS);
    assert!(opened.is_err(), "the migration cannot succeed, so the open must not: {opened:?}");

    let conn = Connection::open(&path).expect("reopening");
    let version: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("reading the version");
    assert_eq!(version, 2, "still v2, so a later build can still try");

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("preparing")
        .query_map([], |row| row.get(0))
        .expect("querying")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(tables, vec!["assignment".to_owned()], "no half-renamed table left behind");

    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('assignment') WHERE name LIKE '%_idx'")
        .expect("preparing")
        .query_map([], |row| row.get(0))
        .expect("querying")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(columns, vec!["start_idx".to_owned(), "end_idx".to_owned()], "and v2 rows intact");
}
