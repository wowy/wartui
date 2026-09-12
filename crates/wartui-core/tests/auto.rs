//! The Phase 5 milestone: the fleet partitioning itself, against a simulated
//! fleet on a fake clock.
//!
//! The planner's arithmetic is property-tested in `wartui-proto`. What cannot be
//! tested there is whether a partition survives the trip through the engine, the wire
//! format, a node's `!=` on the epoch, and back out as the channels it scans — so six
//! nodes converge on a partition of the US pool and the capture is then read back for
//! any network reported on a channel the pool excludes.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::runtime::{drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::air::RecordKind;
use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun, SCAN_CHANNELS};

const NODES: usize = 6;

/// Wait for something to become true of the published snapshot, or give up.
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

/// Every index a collection of channel sets covers, sorted.
fn covered(sets: &[ChannelSet]) -> Vec<u8> {
    let mut all: Vec<u8> = sets.iter().flat_map(|set| set.indices()).collect();
    all.sort_unstable();
    all
}

/// The same, for the runs that describe a pool.
fn covered_runs(runs: &[IndexRun]) -> Vec<u8> {
    let mut all: Vec<u8> = runs.iter().flat_map(|r| r.start..=r.end).collect();
    all.sort_unstable();
    all
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fleet_left_to_itself_converges_on_a_partition_of_the_us_pool() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // `ble_chance` is high and nothing is ever given the Bluetooth assignment, so a
    // busy room full of advertisers should produce no BLE rows. Checked at the bottom.
    let link = SimTransport::new(SimConfig {
        node_count: u8::try_from(NODES).expect("six fits"),
        speed: 60.0,
        ble_chance: 0.8,
        ..Default::default()
    })
    .start()
    .expect("starting the simulator");

    let started = now();
    let session = SessionInfo { espnow_channel: 6, pool: ChannelPool::Us, ..Default::default() };
    let store = Store::open(&StoreConfig::new(&path), &session, started.unix_ms)
        .expect("opening the store");
    let config = EngineConfig {
        pool: ChannelPool::Us,
        auto: true,
        assignment_base: store.assignment_base(),
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);

    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (_command_tx, command_rx) = mpsc::channel(4);
    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));

    // Nobody asks for any of this: the nodes turn up and each takes its share in the
    // admin window its own heartbeat opens.
    let settled = until(&snapshot_rx, "every node to acknowledge a range", |s| {
        s.nodes.len() == NODES
            && s.nodes.iter().all(|n| {
                n.state
                    .confirmed
                    .is_some_and(|c| n.state.desired.is_some_and(|d| d.counter == c.counter))
            })
    })
    .await;

    let plan = settled.plan.expect("a partition is in force");
    assert_eq!(plan.node_count(), 6);

    let held: Vec<ChannelSet> =
        settled.nodes.iter().map(|n| n.state.confirmed.expect("confirmed").channels).collect();
    // Exact equality proves both things at once: the pool is covered, and no
    // two nodes overlap.
    assert_eq!(covered(&held), covered_runs(ChannelPool::Us.runs()));

    let mut slots: Vec<(u8, u8)> = settled
        .nodes
        .iter()
        .map(|n| {
            let a = n.state.confirmed.expect("confirmed");
            (a.node_index, a.node_count)
        })
        .collect();
    slots.sort_unstable();
    // The stagger slot is computed from these (`src/RadioTuning.cpp:3-13`), so
    // two nodes sharing an index would key up on top of each other.
    assert_eq!(slots, (0..6).map(|i| (i, 6)).collect::<Vec<_>>());

    // A node adopts mid-sweep but finishes the sweep it is on, so give each a couple
    // of clear sweeps before believing what it reports.
    let beats: Vec<u64> = settled.nodes.iter().map(|n| n.state.heartbeats).collect();
    until(&snapshot_rx, "two clear sweeps on the new assignments", |s| {
        s.nodes.iter().zip(&beats).all(|(n, before)| n.state.heartbeats >= before + 3)
            && s.tail.iter().filter(|e| e.kind == RecordKind::Wifi).count() >= 20
    })
    .await;

    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    // Every row, with no cut-off: a wartui node parks until assigned, so there is no
    // burst of all-forty-channel observations from before the plan landed.
    let conn = open_readonly(&path).expect("reopening the capture");
    let mut query = conn
        .prepare("SELECT DISTINCT channel FROM observation WHERE kind = 'wifi'")
        .expect("a valid query");
    let channels: Vec<u16> = query
        .query_map([], |row| row.get(0))
        .expect("running the query")
        .collect::<Result<_, _>>()
        .expect("reading the rows");

    assert!(!channels.is_empty(), "the fleet is still reporting networks");
    // The six the US pool leaves out.
    for excluded in [12, 13, 14, 169, 173, 177] {
        assert!(
            !channels.contains(&excluded),
            "channel {excluded} is outside the US pool but was reported: {channels:?}"
        );
    }
    // Stronger, and it costs nothing: every channel reported is one the pool
    // actually contains.
    let allowed: Vec<u16> = covered_runs(ChannelPool::Us.runs())
        .iter()
        .map(|i| u16::from(SCAN_CHANNELS[usize::from(*i)]))
        .collect();
    for channel in &channels {
        assert!(allowed.contains(channel), "channel {channel} is not in the US pool");
    }

    // Nobody was given the Bluetooth assignment, in a room where an advertiser turns
    // up on four dwells in five. A node scanning because its build can would show here.
    let ble: i64 = conn
        .query_row("SELECT count(*) FROM observation WHERE kind = 'ble'", [], |row| row.get(0))
        .expect("counting BLE rows");
    assert_eq!(ble, 0, "no node was asked to scan Bluetooth");
}
