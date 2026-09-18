//! The store and the export, end to end through a real SQLite file.
//!
//! The export's job is to fold a network's many sightings into recapture
//! windows and pick one row per window, and to format it the way WiGLE will
//! accept. Both halves are easy to get subtly wrong and impossible to notice
//! from the outside — a file WiGLE rejects looks exactly like a file it
//! accepts until it is uploaded.

use std::time::Duration;

use rusqlite::Connection;
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::position::{Fix, PositionSource};
use wartui_core::record::{
    AdminOutcome, AssignmentSent, BridgeSeen, Heartbeat, NodeSeen, Observation, Record,
};
use wartui_core::store::{
    Checkpoint, SCHEMA_VERSION, SessionInfo, Store, StoreConfig, open_readonly,
};
use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;
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
fn a_session_records_its_pool_under_the_stored_spelling() {
    // Lowercase, and deliberately not `ChannelPool`'s `Display`: captures on
    // disk carry these strings, so the two spellings are separate on purpose.
    for (pool, spelling) in
        [(ChannelPool::Us, "us"), (ChannelPool::Eu, "eu"), (ChannelPool::All, "all")]
    {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wartui.db");
        let config = StoreConfig::new(&path);
        let session = SessionInfo { espnow_channel: 6, pool, notes: None };
        Store::open(&config, &session, EPOCH_MS).expect("opening the store").close();

        let conn = open_readonly(&path).expect("reopening read-only");
        let stored: String = conn
            .query_row("SELECT channel_pool FROM session", [], |row| row.get(0))
            .expect("the session row");
        assert_eq!(stored, spelling, "{pool:?}");
    }
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

    assert_eq!(summary.rows, 1);
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
    assert_eq!(summary, wartui_core::export::ExportSummary { rows: 1, unpositioned: 1 });
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
    assert_eq!(summary, wartui_core::export::ExportSummary { rows: 1, unpositioned: 0 });
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

/// The OpenRoaming triple, the widest roaming consortium element anybody real
/// beacons.
const OPEN_ROAMING: [u8; 17] = [
    0x02, 0x55, //
    0x5A, 0x03, 0xBA, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x20, 0x00,
];

#[test]
fn a_passpoint_row_exports_its_roaming_consortium() {
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
fn a_window_keeps_a_roaming_consortium_its_strongest_sighting_lacked() {
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
fn a_window_keeps_a_manufacturer_identifier_a_later_weaker_sighting_carried() {
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
fn a_ble_row_exports_its_manufacturer_identifier() {
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
fn a_background_checkpointer_copies_and_truncates_the_wal_and_every_row_survives() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(5);
    // Any WAL at all is past the limit, so every pass that catches up is followed by a
    // truncation, and the last one, with the writer gone, always catches up.
    config.checkpoint = Checkpoint::Background { every: Duration::ZERO, truncate_at: 0 };
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

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
fn a_running_store_says_how_many_of_its_checkpoints_have_caught_up() {
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
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

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
fn a_store_checkpointing_inline_has_no_passes_to_count() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_rows = 10;
    config.batch_interval = Duration::from_secs(60);
    config.checkpoint = Checkpoint::Inline;
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

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
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

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
fn a_checkpoint_right_after_each_commit_lets_the_writer_rewind_the_wal_itself() {
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
fn a_commit_too_soon_after_a_checkpoint_still_gets_one_when_the_fleet_goes_quiet() {
    // The writer's own checkpoint is off, so a wake-up the checkpointer passes over would
    // leave that commit unsynced until the next commit, which a quiet fleet never sends.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_interval = Duration::from_millis(10);
    config.checkpoint =
        Checkpoint::Background { every: Duration::from_millis(200), truncate_at: u64::MAX };
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");

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
fn by_default_the_store_checkpoints_from_its_own_thread_after_every_commit() {
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
fn an_inline_checkpoint_leaves_it_to_sqlite_and_no_thread_reports_any() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut config = StoreConfig::new(dir.path().join("wartui.db"));
    config.batch_interval = Duration::from_millis(10);
    config.checkpoint = Checkpoint::Inline;
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
    let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");
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
fn a_new_database_has_no_index_on_bssid_and_exports_all_the_same() {
    // The index cost the card fourteen times the writes and made export slower; the
    // export's scan and sort must still find every network without it.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -70, EPOCH_MS, fixed(37.0, -122.0)),
            observation(OTHER, [0xAA; 6], -50, EPOCH_MS + 1_000, fixed(37.1, -122.1)),
            observation(NODE, [0xBB; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );
    assert!(!has_bssid_index(&dir.path().join("wartui.db")));

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 2, "{csv}");
    assert!(csv.contains(",-50,37.1,"), "still the strongest sighting: {csv}");
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
fn first_seen_comes_from_the_window_s_own_first_sighting_even_unpositioned() {
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
fn a_re_hearing_past_the_window_exports_a_second_row() {
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
fn a_re_hearing_exactly_an_hour_later_is_still_the_same_row() {
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
fn rows_are_ordered_by_when_their_windows_opened() {
    // The fold visits networks in address order; the file must not.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(OTHER, [0xBB; 6], -60, EPOCH_MS + 60_000, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
        ],
    );

    let (csv, _) = export(&conn);
    let rows: Vec<&str> = csv.lines().skip(2).collect();
    assert!(rows[0].starts_with("AA:AA:AA:AA:AA:AA,"), "{}", rows[0]);
    assert!(rows[1].starts_with("BB:BB:BB:BB:BB:BB,"), "{}", rows[1]);
}

#[test]
fn two_windows_of_one_network_interleave_with_another_in_time_order() {
    // The fold finishes AA's windows before it reaches BB, so an order by network
    // would write AA, AA, BB. The file is ordered by window start.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xBB; 6], -60, EPOCH_MS + 1_800_000, fixed(37.0, -122.0)),
            observation(NODE, [0xAA; 6], -60, EPOCH_MS + 7_200_000, fixed(37.0, -122.0)),
        ],
    );

    let (csv, summary) = export(&conn);
    assert_eq!(summary.rows, 3, "{csv}");
    let macs: Vec<&str> = csv.lines().skip(2).map(|row| &row[..17]).collect();
    assert_eq!(macs, ["AA:AA:AA:AA:AA:AA", "BB:BB:BB:BB:BB:BB", "AA:AA:AA:AA:AA:AA"]);
}

#[test]
fn an_export_run_twice_on_one_connection_writes_the_same_file() {
    // The sort goes through a temporary table on the caller's connection, which a
    // second export must start afresh rather than add to.
    let dir = tempfile::tempdir().expect("temp dir");
    let conn = write(
        &dir,
        vec![
            observation(NODE, [0xAA; 6], -60, EPOCH_MS, fixed(37.0, -122.0)),
            observation(NODE, [0xBB; 6], -60, EPOCH_MS + 60_000, fixed(37.0, -122.0)),
        ],
    );

    let first = export(&conn);
    let second = export(&conn);
    assert_eq!(first.1.rows, 2);
    assert_eq!(first, second);
}

#[test]
fn a_database_from_another_wartui_is_refused_rather_than_written_into() {
    // `CREATE TABLE IF NOT EXISTS` no-ops against a foreign file's tables instead of
    // failing, so without this the build appends rows of the wrong shape and stamps
    // the marker to its own, leaving neither build able to tell it had happened.
    //
    // 1 is deliberately not in this list: it is the marker this build writes
    for found in [99, 2] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wartui.db");
        let other = Connection::open(&path).expect("creating");
        other.pragma_update(None, "user_version", found).expect("stamping");
        drop(other);

        let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, notes: None };
        let opened = Store::open(&StoreConfig::new(&path), &session, EPOCH_MS);
        assert!(
            matches!(
                opened,
                Err(wartui_core::store::StoreError::SchemaMismatch { found: f, ours })
                    if f == found && ours == SCHEMA_VERSION
            ),
            "expected v{found} to be refused, got {opened:?}"
        );

        // The export path refuses it too, and for the same reason.
        assert!(open_readonly(&path).is_err(), "v{found} must not export either");

        let still: i32 = Connection::open(&path)
            .expect("reopening")
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("reading the version");
        assert_eq!(still, found, "and the marker must be left alone");
    }
}

#[test]
fn a_fresh_database_is_stamped_with_this_build_s_schema_version() {
    // 0 is what an empty file reads, and it is the one marker that is not a refusal:
    // there is nothing in the file to be incompatible with. The stamp is what makes
    // the next open recognise it.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    open_at(&path).close();

    let conn = open_readonly(&path).expect("reopening");
    let version: i32 =
        conn.pragma_query_value(None, "user_version", |row| row.get(0)).expect("the version");
    assert_eq!(version, SCHEMA_VERSION);
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
