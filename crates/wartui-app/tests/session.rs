//! A capture started, followed and stopped through a session, over the simulated fleet, and the
//! controls a front end steers it with.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use wartui_app::session::{Controls, Refused, Session, SessionConfig};
use wartui_app::settings::{Config, Link, Settings, SettingsStore};
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::engine::{Command, Snapshot};
use wartui_core::gps::Gps;
use wartui_core::position::{DEFAULT_MAX_AGE, PositionChain, Tier};
use wartui_core::store::{StoreConfig, StoreError, open_readonly};
use wartui_core::ubx;
use wartui_proto::link::{BridgeToHost, HostToBridge};

use common::{Fake, OTHER_BRIDGE, snapshot};

/// The longest any wait below may take before the test fails rather than hangs.
const WAIT: Duration = Duration::from_secs(30);

/// A simulated fleet of `nodes`, fast-forwarded so a few seconds cover whole sweeps.
fn fleet(nodes: u8) -> LinkHandle {
    SimTransport::new(SimConfig { node_count: nodes, speed: 60.0, ..Default::default() })
        .start()
        .expect("starting the simulator")
}

/// A capture into `db`, with a fixed position so every row is placed.
fn config(db: &Path) -> SessionConfig {
    SessionConfig {
        store: StoreConfig::new(db),
        notes: None,
        test_data: true,
        link: None,
        position: PositionChain::new(vec![Tier::fixed(0.0, 0.0, None)], DEFAULT_MAX_AGE),
        wait_for_fix: false,
        record_raw: false,
        excluded_ssids: Vec::new(),
        gps_rate_hz: ubx::DEFAULT_RATE_HZ,
        build: None,
    }
}

/// Settings kept in `store`, holding nothing yet, with the bridge `memory` holds.
fn kept(store: &Arc<Fake>, memory: &BridgeMemory) -> Settings {
    Settings::new(Some(store.clone() as Arc<dyn SettingsStore>), Config::default(), memory.clone())
}

/// Wait until `done` holds, checking every simulated tenth of a second.
async fn until(mut done: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the condition held in time");
}

/// Wait for a snapshot `seen` holds for.
async fn seen(session: &Session, seen: impl FnMut(&Snapshot) -> bool) {
    let mut snapshots = session.snapshots();
    let mut seen = seen;
    timeout(WAIT, snapshots.wait_for(|snapshot| seen(snapshot)))
        .await
        .expect("a snapshot in time")
        .expect("the capture is running");
}

#[tokio::test(start_paused = true)]
async fn session_records_capture_when_simulated_fleet_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("wartui.db");
    let session =
        Session::start(config(&db), fleet(3), None, None, Settings::default()).expect("starting");

    seen(&session, |snapshot| snapshot.nodes.len() == 3 && snapshot.counters.observations > 0)
        .await;
    let end = timeout(WAIT, session.stop()).await.expect("stopped in time").expect("no panic");

    assert!(end.report.written > 0, "rows reached the store: {:?}", end.report);
    let conn = open_readonly(&db).expect("reopening the capture");
    let stored: i64 =
        conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).expect("counting");
    assert!(stored > 0, "observations reached the disk");
    let ended: Option<i64> =
        conn.query_row("SELECT ended_at FROM capture", [], |r| r.get(0)).expect("capture row");
    assert!(ended.is_some(), "a stopped capture closes its capture row out");
}

#[tokio::test(start_paused = true)]
async fn session_records_capture_when_started_on_link_a_session_gave_back() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = dir.path().join("first.db");
    let session =
        Session::start(config(&first), fleet(3), None, None, Settings::default()).expect("first");
    seen(&session, |snapshot| snapshot.bridge.is_some()).await;
    let end = timeout(WAIT, session.stop()).await.expect("stopped in time").expect("no panic");
    assert!(end.bridge.is_some(), "the link still had its bridge announced");

    // The transport announced this bridge once; the second capture is handed the announcement.
    let db = dir.path().join("second.db");
    let session = Session::start(config(&db), end.link, end.bridge, None, Settings::default())
        .expect("second");
    seen(&session, |snapshot| snapshot.nodes.len() == 3 && snapshot.counters.observations > 0)
        .await;
    let end = timeout(WAIT, session.stop()).await.expect("stopped in time").expect("no panic");

    assert!(end.report.written > 0, "rows reached the store: {:?}", end.report);
    let conn = open_readonly(&db).expect("reopening the capture");
    let stored: i64 =
        conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0)).expect("counting");
    assert!(stored > 0, "observations reached the disk");
    let ended: Option<i64> =
        conn.query_row("SELECT ended_at FROM capture", [], |r| r.get(0)).expect("capture row");
    assert!(ended.is_some(), "a stopped capture closes its capture row out");
}

#[tokio::test(start_paused = true)]
async fn session_returns_open_link_when_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = Session::start(
        config(&dir.path().join("wartui.db")),
        fleet(2),
        None,
        None,
        Settings::default(),
    )
    .expect("starting");
    seen(&session, |snapshot| snapshot.bridge.is_some()).await;

    let end = timeout(WAIT, session.stop()).await.expect("stopped in time").expect("no panic");

    let mut link = end.link;
    link.send_bulk(HostToBridge::GetStatus).expect("the link takes a command");
    let answered = timeout(WAIT, async {
        while let Some(event) = link.recv().await {
            if matches!(event, LinkEvent::Message(BridgeToHost::Status { .. })) {
                return true;
            }
        }
        false
    })
    .await
    .expect("an answer in time");
    assert!(answered, "the simulator answered GetStatus over the returned link");
}

#[tokio::test(start_paused = true)]
async fn session_refuses_start_when_capture_exists() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("wartui.db");
    std::fs::write(&db, "").expect("a file in the way");

    let refused = Session::start(config(&db), fleet(1), None, None, Settings::default());

    let Err(Refused { error, link }) = refused else { panic!("started over an existing file") };
    assert!(matches!(error, StoreError::Exists(_)), "{error:?}");
    assert_eq!(std::fs::read(&db).expect("still there"), b"", "the file is left alone");
    link.send_bulk(HostToBridge::GetStatus).expect("the link comes back, still open");
}

#[tokio::test(start_paused = true)]
async fn session_saves_link_when_bridge_announces_itself() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Fake::empty();
    let settings = kept(&store, &BridgeMemory::default()).over(Link::Bluetooth);
    let session =
        Session::start(config(&dir.path().join("wartui.db")), fleet(1), None, None, settings)
            .expect("starting");

    // No front end calls anything: the session follows the capture on its own.
    until(|| store.text().is_some()).await;

    assert_eq!(store.config().bridge.link, Some(Link::Bluetooth));
    assert!(store.text().unwrap_or_default().contains("link = \"bluetooth\""));
    timeout(WAIT, session.stop()).await.expect("stopped in time").expect("no panic");
}

#[tokio::test(start_paused = true)]
async fn session_saves_bridge_once_when_transport_remembers_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let session = Session::start(
        config(&dir.path().join("wartui.db")),
        fleet(1),
        None,
        None,
        kept(&store, &memory),
    )
    .expect("starting");
    seen(&session, |snapshot| snapshot.bridge.is_some()).await;
    assert_eq!(store.saves(), 0, "nothing changed, nothing written");

    memory.remember_on(OTHER_BRIDGE, None); // As a transport does on connecting.
    until(|| store.saves() > 0).await;
    // Several snapshots later, still the one save.
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(store.saves(), 1, "saved once, not every snapshot");
    assert_eq!(store.config().bridge.mac, Some(OTHER_BRIDGE));
    timeout(WAIT, session.stop()).await.expect("stopped in time").expect("no panic");
}

/// A snapshot whose receiver is the bridge's when `via_bridge`, else a host port's.
fn receiver(via_bridge: bool) -> Snapshot {
    let mut snapshot = snapshot(None);
    let mut gps = Gps::bridged(DEFAULT_MAX_AGE).view();
    gps.via_bridge = via_bridge;
    snapshot.gps = Some(gps);
    snapshot
}

#[test]
fn session_asks_for_sky_lists_when_shown_and_receiver_is_the_bridges() {
    let bridge = receiver(true);
    let (tx, mut rx) = mpsc::channel(4);
    let controls = Controls::new(tx, Settings::default());
    controls.follow(&bridge);
    assert!(rx.try_recv().is_err(), "hidden from the start: the engine's default");

    controls.show_sky(true);
    controls.follow(&bridge);
    assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(true)));
    controls.follow(&bridge);
    assert!(rx.try_recv().is_err(), "once per change");

    controls.show_sky(false);
    controls.follow(&bridge);
    assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(false)));
    controls.follow(&bridge);
    assert!(rx.try_recv().is_err());
}

#[test]
fn session_asks_for_no_sky_lists_when_receiver_is_a_host_port_or_sky_hidden() {
    let (tx, mut rx) = mpsc::channel(4);
    let controls = Controls::new(tx, Settings::default());
    controls.follow(&receiver(true));
    assert!(rx.try_recv().is_err(), "nothing shows the sky");

    controls.show_sky(true);
    controls.follow(&receiver(false));
    assert!(rx.try_recv().is_err(), "the bridge's receiver is not the one shown");

    // The receiver read turns to the bridge's while the sky shows, and back.
    controls.follow(&receiver(true));
    assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(true)));
    controls.follow(&receiver(false));
    assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(false)));
}

#[test]
fn controls_retry_sky_lists_when_command_queue_is_full() {
    let bridge = receiver(true);
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(Command::StopScan).expect("room for one");
    let controls = Controls::new(tx, Settings::default());
    controls.show_sky(true);

    controls.follow(&bridge);
    assert_eq!(rx.try_recv(), Ok(Command::StopScan), "the full queue took nothing more");
    assert!(rx.try_recv().is_err());
    controls.follow(&bridge);
    assert_eq!(rx.try_recv(), Ok(Command::SetGpsSky(true)));
}

#[test]
fn controls_report_busy_when_command_queue_is_full() {
    let (tx, mut rx) = mpsc::channel(1);
    let controls = Controls::new(tx, Settings::default());
    assert_eq!(controls.command(Command::StopScan), Ok(()));

    assert!(controls.command(Command::FormatSd).is_err(), "the queue is full");

    assert_eq!(rx.try_recv(), Ok(Command::StopScan));
    assert!(rx.try_recv().is_err(), "the refused command went nowhere");
    drop(rx);
    assert!(controls.command(Command::StopScan).is_err(), "and a stopped engine takes none");
}
