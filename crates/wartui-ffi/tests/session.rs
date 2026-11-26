//! `Session`, a capture started over a `Link` and followed as numbered views: over the simulator,
//! and over a loopback radio to a `WireSim`, the whole path a phone takes.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use wartui_app::view::ReceiverSummary;
use wartui_core::feed::{LocationFix, Pushed};
use wartui_core::position::PositionSource;
use wartui_core::store::{is_simulated, open_readonly};
use wartui_ffi::{
    Link, LocationFeed, PositionSources, Session, Settings, StartError, SteerError, StopError,
};

use common::{Loopback, WAIT, capture, nowhere, start, three_nodes, until};

#[tokio::test]
async fn session_shows_the_fleet_when_link_is_simulated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let session = start(&dir.path().join("capture.db"), &link).await;
    until(&session, |view| three_nodes(view) && view.link.up).await;
    session.stop().await.expect("stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_shows_the_fleet_when_radio_loops_back_to_a_wire_sim() {
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = Settings::unsaved();
    let link = Link::bluetooth(Loopback::radio(), Arc::clone(&settings), None).expect("opened");
    assert!(!link.is_simulated());
    let session = Session::start(
        capture(&dir.path().join("capture.db")),
        Arc::clone(&link),
        settings,
        nowhere(),
        None,
    )
    .await
    .expect("the capture starts");
    let viewed = until(&session, |view| three_nodes(view) && view.bridge.is_some()).await;
    assert!(viewed.view.link.up);
    let end = session.stop().await.expect("stopped");
    assert!(end.written > 0, "{end:?}");
    link.close().await.expect("closed");
}

#[tokio::test]
async fn session_places_rows_by_platform_when_a_fix_is_pushed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let feed = LocationFeed::new();
    let position =
        PositionSources { bridge_receiver: false, location: Some(Arc::clone(&feed)), fixed: None };
    let session = Session::start(
        capture(&dir.path().join("capture.db")),
        Link::simulated(3),
        Settings::unsaved(),
        position,
        None,
    )
    .await
    .expect("the capture starts");
    until(&session, |view| view.link.up).await;
    // Synthetic, mid-ocean, taken now.
    let fix = LocationFix {
        lat: 10.0,
        lon: -40.0,
        alt_msl: None,
        accuracy_m: Some(5.0),
        at_ms: chrono_now_ms(),
        mock: false,
    };
    assert_eq!(feed.push(fix), Pushed::Taken);
    until(&session, |view| {
        view.position.located && view.position.source == PositionSource::Platform
    })
    .await;
    assert_eq!(feed.state().taken, 1);
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_holds_test_data_when_link_is_simulated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("capture.db");
    let session = start(&db, &Link::simulated(3)).await;
    until(&session, |view| view.link.up).await;
    let end = session.stop().await.expect("stopped");
    assert!(end.written > 0, "{end:?}");
    let conn = open_readonly(&db).expect("reopening the capture");
    assert!(is_simulated(&conn).expect("the capture row"));
}

#[tokio::test]
async fn session_gives_link_back_when_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let first = start(&dir.path().join("first.db"), &link).await;
    until(&first, |view| view.bridge.is_some()).await;
    first.stop().await.expect("stopped");

    // The link announced its bridge once; the second capture is handed the announcement.
    let second = start(&dir.path().join("second.db"), &link).await;
    until(&second, |view| three_nodes(view) && view.bridge.is_some()).await;
    second.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_gives_link_back_when_dropped_without_stop() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let first = dir.path().join("first.db");
    let session = start(&first, &link).await;
    until(&session, |view| view.link.up).await;
    drop(session);

    // Stopped in the background: the link comes back, and the capture is closed out.
    let second = timeout(WAIT, async {
        loop {
            let db = dir.path().join("second.db");
            match Session::start(
                capture(&db),
                Arc::clone(&link),
                Settings::unsaved(),
                nowhere(),
                None,
            )
            .await
            {
                Ok(session) => return session,
                Err(StartError::LinkInUse) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(error) => panic!("{error:?}"),
            }
        }
    })
    .await
    .expect("the link came back in time");
    let conn = open_readonly(&first).expect("reopening the first capture");
    let ended: Option<i64> =
        conn.query_row("SELECT ended_at FROM capture", [], |row| row.get(0)).expect("capture row");
    assert!(ended.is_some(), "the dropped capture was closed out");
    second.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_refuses_start_when_link_is_lent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let session = start(&dir.path().join("first.db"), &link).await;
    let refused = Session::start(
        capture(&dir.path().join("second.db")),
        Arc::clone(&link),
        Settings::unsaved(),
        nowhere(),
        None,
    )
    .await;
    assert!(matches!(refused, Err(StartError::LinkInUse)), "{refused:?}");
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_refuses_start_and_keeps_link_when_capture_exists() {
    let dir = tempfile::tempdir().expect("temp dir");
    let taken = dir.path().join("taken.db");
    std::fs::write(&taken, b"").expect("a file in the way");
    let link = Link::simulated(3);
    let refused =
        Session::start(capture(&taken), Arc::clone(&link), Settings::unsaved(), nowhere(), None)
            .await;
    let Err(StartError::Exists { path }) = refused else { panic!("{refused:?}") };
    assert_eq!(path, taken.display().to_string());

    let session = start(&dir.path().join("fresh.db"), &link).await;
    until(&session, |view| view.link.up).await;
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_fails_stop_when_already_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3)).await;
    session.stop().await.expect("stopped");
    assert!(matches!(session.stop().await, Err(StopError::AlreadyStopped)));
}

#[tokio::test]
async fn session_ends_views_when_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3)).await;
    until(&session, |view| view.link.up).await;
    session.stop().await.expect("stopped");
    let last = session.view().seq;
    let after = timeout(WAIT, session.next_view(last)).await.expect("ended in time");
    assert_eq!(after, None);
}

#[tokio::test]
async fn session_starts_scan_when_told_not_to_wait_for_a_fix() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Defaults, which wait for a fix.
    let settings = Settings::unsaved();
    // Never pushed: indoors, or location denied.
    let position = PositionSources {
        bridge_receiver: false,
        location: Some(LocationFeed::new()),
        fixed: None,
    };
    let session = Session::start(
        capture(&dir.path().join("capture.db")),
        Link::simulated(3),
        settings,
        position,
        None,
    )
    .await
    .expect("the capture starts");
    until(&session, |view| view.link.up && view.awaiting_fix).await;
    session.start_without_fix().expect("taken");
    until(&session, |view| !view.awaiting_fix).await;
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_takes_no_command_when_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3)).await;
    session.stop().await.expect("stopped");
    assert!(matches!(session.start_without_fix(), Err(SteerError::Busy)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_shows_no_bridge_receiver_when_wire_sim_bridge_has_none() {
    // The simulator says so right after it announces itself, before the central attaches.
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = Settings::unsaved();
    let link = Link::bluetooth(Loopback::radio(), Arc::clone(&settings), None).expect("opened");
    let position = PositionSources { bridge_receiver: true, location: None, fixed: None };
    let session = Session::start(
        capture(&dir.path().join("capture.db")),
        Arc::clone(&link),
        settings,
        position,
        None,
    )
    .await
    .expect("the capture starts");
    until(&session, |view| {
        view.receiver.as_ref().is_some_and(|gps| gps.summary == ReceiverSummary::NoReceiver)
    })
    .await;
    session.stop().await.expect("stopped");
    link.close().await.expect("closed");
}

/// Unix milliseconds now, the clock a phone stamps a fix with.
fn chrono_now_ms() -> i64 {
    wartui_core::runtime::now().unix_ms
}
