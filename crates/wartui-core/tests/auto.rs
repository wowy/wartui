//! The Phase 5 milestone: the fleet partitioning itself, against a simulated
//! fleet on a fake clock.
//!
//! Two things are being checked, and only the second of them needs a fleet.
//! The planner's arithmetic is already property-tested in `wartui-proto`; what
//! cannot be tested there is whether the partition survives the trip through
//! the engine, the wire format, a node's `!=` comparison on the epoch, and back
//! out as the channels that node then scans. That is the whole chain, and the
//! only evidence a real node offers of what it is scanning is what it reports.
//!
//! So: converge six nodes on a partition of the US pool, then read the capture
//! back and require that not one network was reported on a channel the pool
//! excludes. That is the same check to make against real hardware, where the
//! excluded channels — 12, 13, 14, 169, 173 and 177 — are excluded because the
//! firmware scans *actively* and would be transmitting on them.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::runtime::{drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::air::RecordKind;
use wartui_proto::plan::{ChannelPool, IndexRun, SCAN_CHANNELS};

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

/// Every index a set of ranges covers, sorted.
fn covered(ranges: &[IndexRun]) -> Vec<u8> {
    let mut all: Vec<u8> = ranges.iter().flat_map(|r| r.start..=r.end).collect();
    all.sort_unstable();
    all
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fleet_left_to_itself_converges_on_a_partition_of_the_us_pool() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");

    // A busy neighbourhood: BLE advertisers rotate their addresses, so they
    // never dedup, and they are what pushes Wi-Fi networks back out of each
    // node's 200-entry ring to be reported again. Without them a fleet goes
    // quiet after its first sweep and there is nothing left to check.
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

    // Nobody asks for any of this. The nodes turn up, the engine cuts the pool
    // across them, and each one takes its share in the admin window its own
    // heartbeat opens.
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
    assert!(!plan.rotates(), "six nodes hold both runs at once");

    let ranges: Vec<IndexRun> =
        settled.nodes.iter().map(|n| n.state.confirmed.expect("confirmed").range).collect();
    // Exact equality proves three things at once: the pool is covered, no two
    // nodes overlap, and no range straddles the gap at indices 11-13 — which
    // `MSG_ADMIN` could not express in the first place.
    assert_eq!(covered(&ranges), covered(ChannelPool::Us.runs()));

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

    // A node adopts an assignment mid-sweep but finishes the sweep it is on, so
    // give every one of them a couple of clear sweeps before believing that
    // what it reports is what it was assigned.
    let beats: Vec<u64> = settled.nodes.iter().map(|n| n.state.heartbeats).collect();
    let clear = until(&snapshot_rx, "two clear sweeps on the new ranges", |s| {
        s.nodes.iter().zip(&beats).all(|(n, before)| n.state.heartbeats >= before + 3)
    })
    .await;
    let mark = clear.now_ms;

    // And wait until the fleet is actually reporting Wi-Fi again: a node that
    // has gone quiet because everything is in its dedup ring would satisfy any
    // assertion about what it reports.
    until(&snapshot_rx, "wi-fi to be reported from the assigned ranges", |s| {
        s.tail.iter().filter(|e| e.kind == RecordKind::Wifi && e.rx_at_ms > mark).count() >= 20
    })
    .await;

    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    let conn = open_readonly(&path).expect("reopening the capture");
    let mut query = conn
        .prepare("SELECT DISTINCT channel FROM observation WHERE kind = 'wifi' AND rx_at > ?1")
        .expect("a valid query");
    let channels: Vec<u16> = query
        .query_map([mark], |row| row.get(0))
        .expect("running the query")
        .collect::<Result<_, _>>()
        .expect("reading the rows");

    assert!(!channels.is_empty(), "the fleet is still reporting networks");
    // The pool exists because every scan the firmware performs is active: a
    // node assigned a channel transmits probe requests on it. These six are the
    // ones the US pool leaves out.
    for excluded in [12, 13, 14, 169, 173, 177] {
        assert!(
            !channels.contains(&excluded),
            "channel {excluded} is outside the US pool but was reported: {channels:?}"
        );
    }
    // Stronger, and it costs nothing: every channel reported is one the pool
    // actually contains.
    let allowed: Vec<u16> = covered(ChannelPool::Us.runs())
        .iter()
        .map(|i| u16::from(SCAN_CHANNELS[usize::from(*i)]))
        .collect();
    for channel in &channels {
        assert!(allowed.contains(channel), "channel {channel} is not in the US pool");
    }
}
