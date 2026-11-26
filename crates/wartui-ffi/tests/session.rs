//! `Session`, a capture started over a `Link` and followed as numbered views: over the simulator,
//! and over a loopback radio to a `WireSim`, the whole path a phone takes.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use wartui_core::feed::{LocationFix, Pushed};
use wartui_core::position::PositionSource;
use wartui_core::store::{is_simulated, open_readonly};
use wartui_ffi::{Link, LocationFeed, PositionSources, Session, Settings, StartError, StopError};

use common::{Loopback, WAIT, capture, nowhere, start, three_nodes, until};

#[tokio::test]
async fn session_shows_the_fleet_when_link_is_simulated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let session = start(&dir.path().join("capture.db"), &link);
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
    let session = start(&db, &Link::simulated(3));
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
    let first = start(&dir.path().join("first.db"), &link);
    until(&first, |view| view.bridge.is_some()).await;
    first.stop().await.expect("stopped");

    // The link announced its bridge once; the second capture is handed the announcement.
    let second = start(&dir.path().join("second.db"), &link);
    until(&second, |view| three_nodes(view) && view.bridge.is_some()).await;
    second.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_gives_link_back_when_dropped_without_stop() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let first = dir.path().join("first.db");
    let session = start(&first, &link);
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
            ) {
                Ok(session) => return session,
                Err(StartError::LinkLent) => tokio::time::sleep(Duration::from_millis(50)).await,
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
    let session = start(&dir.path().join("first.db"), &link);
    let refused = Session::start(
        capture(&dir.path().join("second.db")),
        Arc::clone(&link),
        Settings::unsaved(),
        nowhere(),
        None,
    );
    assert!(matches!(refused, Err(StartError::LinkLent)), "{refused:?}");
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_refuses_start_and_keeps_link_when_capture_exists() {
    let dir = tempfile::tempdir().expect("temp dir");
    let taken = dir.path().join("taken.db");
    std::fs::write(&taken, b"").expect("a file in the way");
    let link = Link::simulated(3);
    let refused =
        Session::start(capture(&taken), Arc::clone(&link), Settings::unsaved(), nowhere(), None);
    let Err(StartError::Exists { path }) = refused else { panic!("{refused:?}") };
    assert_eq!(path, taken.display().to_string());

    let session = start(&dir.path().join("fresh.db"), &link);
    until(&session, |view| view.link.up).await;
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_fails_stop_when_already_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3));
    session.stop().await.expect("stopped");
    assert!(matches!(session.stop().await, Err(StopError::AlreadyStopped)));
}

#[tokio::test]
async fn session_ends_views_when_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3));
    until(&session, |view| view.link.up).await;
    session.stop().await.expect("stopped");
    let last = session.view().seq;
    let after = timeout(WAIT, session.next_view(last)).await.expect("ended in time");
    assert_eq!(after, None);
}

/// Unix milliseconds now, the clock a phone stamps a fix with.
fn chrono_now_ms() -> i64 {
    wartui_core::runtime::now().unix_ms
}
