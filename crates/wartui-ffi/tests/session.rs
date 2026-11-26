//! `Session`, a capture started over a `Link` and followed as numbered views: over the simulator,
//! and over a loopback radio to a `WireSim`, the whole path a phone takes.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use wartui_app::settings::SsidRefusal;
use wartui_app::view::ReceiverSummary;
use wartui_core::feed::{LocationFix, Pushed};
use wartui_core::position::PositionSource;
use wartui_core::store::{is_simulated, open_readonly};
use wartui_ffi::{
    Link, LocationFeed, PositionSources, Saved, Session, Settings, StartError, SteerError,
    StopError,
};
use wartui_proto::link::PowerOutcome;
use wartui_proto::plan::ChannelPool;

use common::{
    Loopback, Memory, WAIT, capture, nowhere, saving, start, three_nodes, unmoved, until,
};

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

/// A pool other than `pool`.
fn another(pool: ChannelPool) -> ChannelPool {
    if pool == ChannelPool::Eu { ChannelPool::Us } else { ChannelPool::Eu }
}

#[tokio::test]
async fn session_apply_sends_pool_and_saves_when_row_moved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("an empty store opens");
    let session = saving(&dir.path().join("capture.db"), &Link::simulated(3), &settings).await;
    let shown = until(&session, |view| view.settings.pool.is_some()).await.view;
    let pool = another(shown.settings.pool.expect("a pool"));

    let edit = wartui_ffi::SettingsEdit { pool: Some(pool), ..unmoved(&shown, &settings) };
    let applied = session.apply(edit).expect("applied");
    assert_eq!(applied.saved, Saved::At { place: "memory".to_owned() });
    assert!(store.text().is_some(), "the store was saved");
    until(&session, |view| view.settings.pool == Some(pool)).await;
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_apply_refuses_edit_when_fan_duty_is_out_of_range() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("an empty store opens");
    let session = saving(&dir.path().join("capture.db"), &Link::simulated(3), &settings).await;
    let shown = until(&session, |view| view.settings.pool.is_some()).await.view;
    let pool = shown.settings.pool;

    let edit = wartui_ffi::SettingsEdit {
        pool: pool.map(another),
        fan_duty_pct: Some(101),
        ..unmoved(&shown, &settings)
    };
    let refused = session.apply(edit);
    assert!(matches!(refused, Err(SteerError::FanDuty { pct: 101 })), "{refused:?}");
    assert_eq!(store.text(), None, "nothing saved");
    // Room for a pool sent to land, as it does when an edit is applied.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(session.view().view.settings.pool, pool, "nothing sent");
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_apply_is_busy_when_capture_has_stopped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("an empty store opens");
    let session = saving(&dir.path().join("capture.db"), &Link::simulated(3), &settings).await;
    let shown = until(&session, |view| view.link.up).await.view;
    session.stop().await.expect("stopped");

    let edit = wartui_ffi::SettingsEdit { wait_for_fix: false, ..unmoved(&shown, &settings) };
    assert!(matches!(session.apply(edit), Err(SteerError::Busy)));
    assert!(matches!(session.apply_excluded(vec!["HomeNet".to_owned()]), Err(SteerError::Busy)));
    assert_eq!(store.text(), None, "nothing saved");
}

#[tokio::test]
async fn session_apply_excluded_saves_only_the_list_when_capture_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Memory::holding("[gps]\nwait-for-fix = false\n");
    let settings = Settings::open(store.clone(), None).expect("the store opens");
    let session = saving(&dir.path().join("capture.db"), &Link::simulated(3), &settings).await;
    until(&session, |view| view.link.up).await;

    let saved = session.apply_excluded(vec!["HomeNet".to_owned()]).expect("applied");
    assert_eq!(saved, Saved::At { place: "memory".to_owned() });
    let text = store.text().expect("saved");
    let config = wartui_app::settings::parse(&text).expect("valid settings");
    assert_eq!(config.excluded_ssids, ["HomeNet"]);
    assert_eq!(config.gps.wait_for_fix, Some(false), "the rest kept: {text}");
    until(&session, |view| view.settings.excluded_ssids == ["HomeNet"]).await;
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_apply_excluded_refuses_list_when_an_ssid_is_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Memory::holding("excluded-ssids = [\"HomeNet\"]\n");
    let settings = Settings::open(store.clone(), None).expect("the store opens");
    let session = saving(&dir.path().join("capture.db"), &Link::simulated(3), &settings).await;
    until(&session, |view| view.link.up && view.settings.excluded_ssids == ["HomeNet"]).await;

    let refused = session.apply_excluded(vec!["CafeNet".to_owned(), String::new()]);
    let Err(SteerError::ExcludedSsid { ssid, refusal }) = refused else { panic!("{refused:?}") };
    assert_eq!((ssid.as_str(), refusal), ("", SsidRefusal::Empty));
    assert_eq!(store.text().as_deref(), Some("excluded-ssids = [\"HomeNet\"]\n"), "nothing saved");
    // Room for a list sent to land, as it does when one is applied.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(session.view().view.settings.excluded_ssids, ["HomeNet"], "nothing sent");
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_apply_refuses_edit_when_an_ssid_is_listed_twice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("an empty store opens");
    let session = saving(&dir.path().join("capture.db"), &Link::simulated(3), &settings).await;
    let shown = until(&session, |view| view.link.up).await.view;

    let excluded_ssids = vec!["HomeNet".to_owned(), "HomeNet".to_owned()];
    let edit = wartui_ffi::SettingsEdit { excluded_ssids, ..unmoved(&shown, &settings) };
    let refused = session.apply(edit);
    let Err(SteerError::ExcludedSsid { ssid, refusal }) = refused else { panic!("{refused:?}") };
    assert_eq!((ssid.as_str(), refusal), ("HomeNet", SsidRefusal::Listed));
    assert_eq!(store.text(), None, "nothing saved");
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_power_cycle_answers_in_the_view_when_slot_is_named() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3)).await;
    let shown = until(&session, |view| view.nodes.first().is_some_and(|n| n.slot.is_some())).await;
    let slot = shown.view.nodes[0].slot.clone().expect("a slot");

    session.power_cycle(slot.index).expect("taken");
    let answered = until(&session, |view| view.answers.power.is_some()).await;
    let power = answered.view.answers.power.expect("an answer");
    assert_eq!(power.slot, slot);
    assert_eq!(power.outcome, PowerOutcome::Done);
    session.stop().await.expect("stopped");
}

#[tokio::test]
async fn session_power_cycle_refuses_slot_when_it_names_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = start(&dir.path().join("capture.db"), &Link::simulated(3)).await;
    let refused = session.power_cycle(0xFF);
    assert!(matches!(refused, Err(SteerError::NoSuchSlot { slot: 0xFF })), "{refused:?}");
    session.stop().await.expect("stopped");
}

/// Unix milliseconds now, the clock a phone stamps a fix with.
fn chrono_now_ms() -> i64 {
    wartui_core::runtime::now().unix_ms
}
