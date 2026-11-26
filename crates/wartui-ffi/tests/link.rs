//! `Link`, the link to a bridge held between captures.

mod common;

use std::sync::Arc;
use std::time::Duration;

use wartui_core::store::open_readonly;
use wartui_ffi::{Link, LinkError, Session, Settings};

use common::{Loopback, capture, nowhere, start, three_nodes, until};

#[tokio::test]
async fn link_refuses_a_path_when_asked_for_a_bluetooth_bridge() {
    let radio = Loopback::radio();
    let opened = Link::bluetooth(radio, Settings::unsaved(), Some("/dev/ttyACM0".to_owned()));
    let Err(LinkError::NotABridgeName { given }) = opened else {
        panic!("a path is no bridge's name: {opened:?}");
    };
    assert_eq!(given, "/dev/ttyACM0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_stays_caught_up_when_idle_between_captures() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let first = start(&dir.path().join("first.db"), &link).await;
    until(&first, three_nodes).await;
    first.stop().await.expect("stopped");
    // A stopped node heartbeats every second, so an unread link would back up 5 s of them.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let db = dir.path().join("second.db");
    let second = start(&db, &link).await;
    // Each node's heartbeat read, and time for a backlog behind them to be read too.
    until(&second, |view| three_nodes(view) && view.nodes.iter().all(|n| n.firmware.is_some()))
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    second.stop().await.expect("stopped");
    // A node heartbeats at most once a second, so two of its heartbeats read closer together than
    // that came out of a backlog. A loaded host reading late bunches them by far less.
    let conn = open_readonly(&db).expect("reopening the capture");
    let mut rows =
        conn.prepare("SELECT node_mac, rx_at FROM heartbeat ORDER BY node_mac, rx_at").unwrap();
    let beats: Vec<(Vec<u8>, i64)> =
        rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().flatten().collect();
    assert!(!beats.is_empty(), "the nodes heartbeat");
    let bunched = beats.windows(2).filter(|w| w[0].0 == w[1].0 && w[1].1 - w[0].1 < 500).count();
    assert_eq!(bunched, 0, "a backlog was read: {beats:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_drops_the_announcement_when_bridge_goes_while_idle() {
    let dir = tempfile::tempdir().expect("temp dir");
    let settings = Settings::unsaved();
    let (radio, driver) = Loopback::steered();
    let link = Link::bluetooth(radio, Arc::clone(&settings), None).expect("opened");
    let first = Session::start(
        capture(&dir.path().join("first.db")),
        Arc::clone(&link),
        Arc::clone(&settings),
        nowhere(),
        None,
    )
    .await
    .expect("the capture starts");
    until(&first, |view| view.bridge.is_some()).await;
    first.stop().await.expect("stopped");

    driver.go();
    // The parked link reads the drop at once; this is room for it.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second = Session::start(
        capture(&dir.path().join("second.db")),
        Arc::clone(&link),
        settings,
        nowhere(),
        None,
    )
    .await
    .expect("the capture starts");
    // A capture handed a bridge shows it, as the last one known, even once its link is down.
    let shown = tokio::time::timeout(Duration::from_secs(2), async {
        let mut after = 0;
        while let Some(viewed) = second.next_view(after).await {
            if viewed.view.bridge.is_some() {
                return true;
            }
            after = viewed.seq;
        }
        false
    })
    .await;
    assert!(!shown.unwrap_or(false), "a bridge gone while idle is not handed on");
    second.stop().await.expect("stopped");
    link.close().await.expect("closed");
}
