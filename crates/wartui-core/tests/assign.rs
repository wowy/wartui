//! The Phase 4 milestone, run against a simulated fleet.
//!
//! A node heartbeats once per completed sweep and reports nothing at all about
//! what it is scanning, so the only evidence available that an assignment was
//! *adopted* — rather than merely acknowledged — is that the node's heartbeat
//! period changed. Narrowing a node from every channel to exactly one should
//! collapse its sweep by roughly the ratio of the two ranges.
//!
//! This is the same check to make against real hardware, at real speed, with
//! `a` in the fleet view. The simulator makes it a test rather than an evening.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{Command, EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::record::AdminOutcome;
use wartui_core::runtime::{drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::plan::{ChannelPool, IndexRun, NUM_SCAN_CHANNELS};

/// Wait for something to become true of the published snapshot, or give up.
///
/// Polls rather than waits on `changed()` so a condition that is already true
/// does not hang, and so a failure reports the state it gave up on.
async fn until(
    rx: &watch::Receiver<Arc<Snapshot>>,
    what: &str,
    mut ready: impl FnMut(&Snapshot) -> bool,
) -> Arc<Snapshot> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = rx.borrow().clone();
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrowing_a_node_to_one_channel_collapses_its_heartbeat_period() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // One node, fast-forwarded. One because the point of the test is a single
    // node's sweep time, and a second node would only add stagger.
    let link = SimTransport::new(SimConfig { node_count: 1, speed: 60.0, ..Default::default() })
        .start()
        .expect("starting the simulator");

    let started = now();
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::All, ..Default::default() };
    let store = Store::open(&StoreConfig::new(&path), &session, started.unix_ms)
        .expect("opening the store");
    let config = EngineConfig {
        pool: ChannelPool::All,
        assignment_base: store.assignment_base(),
        // The planner off, because this is the by-hand path: with it on, the
        // node would be given the whole pool before the operator says anything
        // and there would be no wide baseline to narrow from.
        auto: false,
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);

    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (command_tx, command_rx) = mpsc::channel(4);
    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));

    // A node that has never heard a core scans all forty channels
    // (`src/WiFiOps.cpp:77-80`), so this is the wide baseline.
    let wide = until(&snapshot_rx, "a settled heartbeat period", |s| {
        s.nodes.first().is_some_and(|n| n.state.heartbeats >= 4)
    })
    .await;
    let wide_period = wide.nodes[0].state.beat_period_ms().expect("a measured period");
    let mac = wide.nodes[0].state.mac;
    assert!(wide.nodes[0].state.confirmed.is_none(), "nothing has assigned it yet");

    command_tx
        .send(Command::Assign { mac, range: IndexRun::new(5, 5) })
        .await
        .expect("the engine is listening");

    // Nothing goes out until the node's next heartbeat opens its admin window,
    // and the node adopts the range only because the epoch differs from the 0
    // it booted with — the `!=` at `src/WiFiOps.cpp:1198`.
    let acked = until(&snapshot_rx, "the assignment to be acknowledged", |s| {
        s.nodes.first().is_some_and(|n| n.state.confirmed.is_some())
    })
    .await;
    let node = &acked.nodes[0];
    assert_eq!(node.state.confirmed.expect("confirmed").range, IndexRun::new(5, 5));
    assert!(!node.state.dirty, "acknowledged, so nothing is owed");
    assert_eq!(acked.counters.admin_acked, 1);
    assert_eq!(acked.counters.admin_failed, 0);

    // Three more sweeps for the median to have forgotten the wide ones. This is
    // the whole milestone: one channel instead of forty, so about a fortieth of
    // the sweep, observable without any access to the node beyond its radio.
    let beats_before = node.state.heartbeats;
    let narrow = until(&snapshot_rx, "three sweeps at the new range", |s| {
        s.nodes.first().is_some_and(|n| n.state.heartbeats >= beats_before + 4)
    })
    .await;
    let narrow_period = narrow.nodes[0].state.beat_period_ms().expect("a measured period");

    assert!(
        narrow_period * 4 < wide_period,
        "a range of one channel out of {NUM_SCAN_CHANNELS} should be far quicker: \
         {wide_period} ms wide, {narrow_period} ms narrow"
    );

    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    let conn = open_readonly(&path).expect("reopening the capture");
    let (outcome, start_idx, end_idx, latency): (String, u8, u8, Option<i64>) = conn
        .query_row(
            "SELECT outcome, start_idx, end_idx, latency_us FROM assignment ORDER BY id DESC",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("an assignment row");
    assert_eq!(outcome, AdminOutcome::Acked.as_str());
    assert_eq!((start_idx, end_idx), (5, 5));
    // The number that settles whether a bridge this dumb can hit a 300 ms
    // window. Measured on the bridge's own clock, from the heartbeat that
    // opened the window to the transmit callback.
    assert!(latency.is_some(), "and carries the latency the whole design turns on");
}
