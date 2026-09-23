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
fn session_record_stores_pool_spelling_when_session_is_created() {
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
fn store_persists_and_reads_all_record_types_when_round_tripped() {
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
fn node_tracker_updates_last_seen_and_preserves_first_seen_when_re_observed() {
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
fn store_persists_all_sightings_without_early_deduplication_when_inserted() {
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
fn export_writer_selects_strongest_sighting_and_earliest_first_seen_when_generating_row() {
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
fn export_writer_emits_valid_wigle_header_when_generating_csv() {
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
fn export_writer_increments_unpositioned_counter_when_sighting_lacks_fix() {
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
fn export_writer_exports_positioned_sighting_when_stronger_sighting_lacks_fix() {
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
fn export_writer_quotes_special_characters_when_exporting_ssid() {
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
fn export_writer_strips_null_padding_when_exporting_legacy_cloaked_ssid() {
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
fn export_writer_formats_ble_record_with_blank_frequency_when_sighting_is_ble() {
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
fn export_writer_formats_roaming_consortium_when_passpoint_sighting_exported() {
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
fn export_writer_preserves_roaming_consortium_when_strongest_sighting_lacked_element() {
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
fn export_writer_preserves_manufacturer_id_when_subsequent_weaker_sighting_carries_it() {
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
fn export_writer_formats_ble_record_with_manufacturer_id_when_sighting_exported() {
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
fn store_drops_records_without_blocking_when_queue_depth_is_exceeded() {
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
fn store_reports_committed_batch_timings_on_close_when_requested() {
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
fn store_omits_batch_timings_when_not_explicitly_requested() {
    // The list grows per commit for as long as a capture runs.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = store(&dir);
    assert_eq!(store.submit(vec![observation(NODE, [0xAA; 6], -60, EPOCH_MS, Fix::none())]), 0);
    let report = store.close();
    assert_eq!(report.written, 1);
    assert!(report.batches.is_empty());
}

#[test]
fn background_checkpointer_flushes_and_truncates_wal_when_configured() {
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
fn store_reports_caught_up_checkpoint_count_when_running() {
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
fn inline_checkpointer_has_zero_passes_when_queried_for_caught_up() {
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
fn background_checkpointer_reduces_wal_growth_when_running_after_each_commit() {
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
fn background_checkpointer_runs_deferred_pass_when_fleet_becomes_quiet() {
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
fn store_defaults_to_background_checkpointer_when_initialized() {
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
fn inline_checkpointer_reports_no_passes_when_configured() {
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
fn store_exports_all_networks_without_bssid_index_when_queried() {
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
fn store_configures_custom_page_size_when_creating_database() {
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
fn export_writer_uses_first_seen_from_earliest_unpositioned_sighting_when_window_is_zero() {
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
fn export_writer_uses_window_first_sighting_timestamp_when_window_is_unpositioned() {
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
fn export_writer_splits_sightings_into_new_row_when_window_interval_exceeded() {
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
fn export_writer_keeps_sighting_in_same_window_when_exactly_at_one_hour_boundary() {
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
fn export_writer_orders_rows_by_window_open_timestamp_when_generating_export() {
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
fn export_writer_interleaves_network_windows_in_chronological_order_when_exported() {
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
fn export_writer_produces_identical_output_when_run_repeatedly_on_connection() {
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
fn store_refuses_database_connection_when_schema_version_mismatches() {
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
fn store_stamps_current_schema_version_when_creating_database() {
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
fn store_persists_bridge_metadata_when_bridge_announces_in_session() {
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
fn store_records_every_assignment_attempt_regardless_of_outcome_when_sent() {
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
fn store_advances_assignment_base_counter_when_session_opens_and_spends() {
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
