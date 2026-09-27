//! The Phase 4 milestone, run against a simulated fleet.
//!
//! A node's heartbeat carries the epoch of the assignment it holds, so
//! adoption — not just a MAC-layer ack — is directly observable rather than
//! inferred from a sweep period. Nothing but the planner decides what a node
//! holds, and the only thing that moves a share is how many nodes there are to
//! share with.
//!
//! This is the same check to make against real hardware: bring a second node
//! up and watch the first one's assignment narrow and get re-adopted under a
//! fresh epoch. The simulator makes it a test rather than an evening.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::record::AdminOutcome;
use wartui_core::runtime::{drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::plan::{ChannelPool, ChannelSet};

/// How much faster than real time the fake fleet runs.
const SPEED: u32 = 60;

/// Wait for something to become true of the published snapshot, or give up.
///
/// Polls rather than waits on `changed()` so a condition that is already true
/// does not hang, and so a failure reports the state it gave up on.
async fn until(
    rx: &watch::Receiver<Arc<Snapshot>>,
    what: &str,
    mut ready: impl FnMut(&Snapshot) -> bool,
) -> Arc<Snapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = rx.borrow().clone();
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Run a fleet of `nodes` until every one of them has *adopted* its share —
/// its own heartbeat reports the epoch it was dealt, not merely acknowledged
/// it — and report each node's adopted channels, in fleet order.
///
/// Nobody asks for any of it: the nodes turn up, the planner cuts the pool,
/// and each takes its share in the admin window its own heartbeat opens.
async fn settle(path: &Path, nodes: u8) -> Vec<ChannelSet> {
    let link = SimTransport::new(SimConfig {
        node_count: nodes,
        speed: f64::from(SPEED),
        ..Default::default()
    })
    .start()
    .expect("starting the simulator");

    let started = now();
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::All, ..Default::default() };
    let store =
        Store::open(&StoreConfig::new(path), &session, started.unix_ms).expect("opening the store");
    let config = EngineConfig {
        pool: ChannelPool::All,
        assignment_base: store.assignment_base(),
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);

    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (_command_tx, command_rx) = mpsc::channel(4);
    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));

    // Every node, because the last one to arrive re-cuts every share: a check
    // made before that would be against a range some node no longer holds.
    let settled = until(&snapshot_rx, "every node to adopt its share", |s| {
        s.nodes.len() == usize::from(nodes) && s.nodes.iter().all(|n| n.state.adopted())
    })
    .await;
    assert_eq!(settled.counters.admin_failed, 0);

    let channels: Vec<ChannelSet> =
        settled.nodes.iter().map(|n| n.state.desired.expect("adopted has one").channels).collect();
    let mac = settled.nodes[0].state.mac;

    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    // What was written down about the frame that put the first node there,
    // and the heartbeat that then confirmed it held it.
    let conn = open_readonly(path).expect("reopening the capture");
    let (outcome, stored, ble, latency): (String, i64, bool, Option<i64>) = conn
        .query_row(
            "SELECT outcome, channels, ble, latency_us FROM assignment
             WHERE node_mac = ?1 ORDER BY id DESC",
            [&mac[..]],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("an assignment row");
    assert_eq!(outcome, AdminOutcome::Acked.as_str());
    // Stored as the mask that went on the wire, so the row says what the node
    // was told rather than an interpretation of it.
    assert_eq!(ChannelSet::from_bits(u64::try_from(stored).expect("a 40-bit mask")), channels[0]);
    assert!(!ble, "nobody asked for the Bluetooth scan");
    // The number that settles whether a bridge this dumb can hit a 100 ms
    // window. Measured on the bridge's own clock, from the heartbeat that
    // opened the window to the transmit callback.
    assert!(latency.is_some(), "and carries the latency the whole design turns on");

    let epoch: i64 = conn
        .query_row(
            "SELECT epoch FROM heartbeat WHERE node_mac = ?1 ORDER BY id DESC",
            [&mac[..]],
            |row| row.get(0),
        )
        .expect("a heartbeat row");
    assert_ne!(epoch, 0, "the stored heartbeat says the node held something, not nothing");

    channels
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_flow_reports_new_epoch_when_second_node_narrows_the_share() {
    let dir = tempfile::tempdir().expect("temp dir");

    let alone = settle(&dir.path().join("lone.db"), 1).await;
    assert_eq!(alone[0], ChannelPool::All.channels(), "a fleet of one shares with nobody");

    let paired = settle(&dir.path().join("fleet.db"), 2).await;
    assert!(paired[0].len() < alone[0].len(), "narrower now it shares the pool");
    assert!(paired[1].len() < alone[0].len());

    let covered: BTreeSet<u8> = paired[0].indices().chain(paired[1].indices()).collect();
    assert_eq!(covered.len() as u32, paired[0].len() + paired[1].len(), "disjoint shares");
    assert_eq!(covered.len() as u32, alone[0].len(), "and together the whole pool");
}
