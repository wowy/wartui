//! The Phase 4 milestone, run against a simulated fleet.
//!
//! A node heartbeats once per completed sweep and reports nothing at all about
//! what it is scanning, so the only evidence available that an assignment was
//! *adopted* — rather than merely acknowledged — is that the node's heartbeat
//! period changed. Nothing but the planner decides what a node holds, and the
//! only thing that moves a share is how many nodes there are to share with: a
//! fleet of one takes the whole pool, and a fleet of ten takes a tenth each.
//! Their sweep periods should differ by about that ratio.
//!
//! This is the same check to make against real hardware, at real speed: bring a
//! second node up and watch the first one's `beat` halve. The simulator makes it
//! a test rather than an evening.
//!
//! The period is a median over the last [`BEAT_WINDOW`] gaps and is not reset on a
//! new range, so every measurement here waits out a whole window of beats *counted
//! from the assignment it is measuring*.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{BEAT_WINDOW, EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::record::AdminOutcome;
use wartui_core::runtime::{drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::plan::{CHANNEL_DWELL_MS, ChannelPool, ChannelSet, NUM_SCAN_CHANNELS};

/// How much faster than real time the fake fleet runs.
///
/// Named because the periods being measured are derived from it: at 60 the
/// node's idle beat is 17 ms, its sweep of the whole table 88 ms and its sweep
/// of one channel 7 ms. All three are far shorter than the 250 ms
/// [`wartui_core::runtime::TICK`] the snapshot is republished on, which is why
/// nothing below infers anything from *when* a snapshot was seen.
const SPEED: u32 = 60;

/// The fleet the narrow measurement is taken from.
///
/// Ten, so each node is dealt about a tenth of the pool — a ratio wide enough
/// that the two periods cannot be confused for each other, and a fleet small
/// enough that every node still settles inside the deadline below.
const FLEET: u8 = 10;

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

/// Run a fleet of `nodes` until the first of them has swept its own share for a
/// whole [`BEAT_WINDOW`], and report what it was dealt and how long that sweep
/// takes.
///
/// Nobody asks for any of it: the nodes turn up, the planner cuts the pool, and
/// each takes its share in the admin window its own heartbeat opens.
async fn sweep(path: &Path, nodes: u8) -> (ChannelSet, u32) {
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

    // The whole fleet, because the last node to arrive re-cuts every share: a
    // period measured before that would be of a range the node no longer holds.
    let settled = until(&snapshot_rx, "every node to acknowledge a share", |s| {
        s.nodes.len() == usize::from(nodes)
            && s.nodes.iter().all(|n| {
                n.state
                    .confirmed
                    .is_some_and(|c| n.state.desired.is_some_and(|d| d.counter == c.counter))
            })
    })
    .await;
    let first = &settled.nodes[0];
    let channels = first.state.confirmed.expect("a share").channels;
    let mac = first.state.mac;

    // A whole window of sweeps, counted from the beat that opened the admin
    // window: a median with any idle beat left in it reports the parked node's
    // 17 ms. How many there were is not a fixed number to skip, since nothing
    // goes out until a heartbeat opens a window.
    let window = u64::try_from(BEAT_WINDOW).expect("a five-deep window fits in a u64");
    let before = first.state.heartbeats;
    let swept = until(&snapshot_rx, "a full window of sweeps of the new share", |s| {
        s.nodes.first().is_some_and(|n| n.state.heartbeats >= before + window)
    })
    .await;
    let period = swept.nodes[0].state.beat_period_ms().expect("a measured period");
    assert_eq!(swept.counters.admin_failed, 0);

    stop_tx.send(()).expect("the capture is still running");
    capture.await.expect("the capture task should not panic");

    // What was written down about the frame that put it there.
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
    assert_eq!(ChannelSet::from_bits(u64::try_from(stored).expect("a 40-bit mask")), channels);
    assert!(!ble, "nobody asked for the Bluetooth scan");
    // The number that settles whether a bridge this dumb can hit a 100 ms
    // window. Measured on the bridge's own clock, from the heartbeat that
    // opened the window to the transmit callback.
    assert!(latency.is_some(), "and carries the latency the whole design turns on");

    (channels, period)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_dealt_a_smaller_share_sweeps_it_faster() {
    let dir = tempfile::tempdir().expect("temp dir");

    let (whole, wide_period) = sweep(&dir.path().join("lone.db"), 1).await;
    assert_eq!(whole, ChannelPool::All.channels(), "a fleet of one shares with nobody");
    // Half a sweep of the whole table, asserted here so a failure says which of the
    // two numbers was wrong rather than only that the ratio was.
    let half_a_sweep = u32::from(NUM_SCAN_CHANNELS) * CHANNEL_DWELL_MS / SPEED / 2;
    assert!(
        wide_period > half_a_sweep,
        "the whole table should take at least {half_a_sweep} ms, not the idle beat: {wide_period} ms"
    );

    let (share, narrow_period) = sweep(&dir.path().join("fleet.db"), FLEET).await;
    assert!(
        share.len() * u32::from(FLEET) <= whole.len() + u32::from(FLEET),
        "a tenth of the pool, give or take the remainder: {} of {}",
        share.len(),
        whole.len()
    );

    // One-tenth of the table instead of all of it, observable with no access to
    // the node but its radio.
    assert!(
        narrow_period * 4 < wide_period,
        "a share of {} channels out of {} should be far quicker: \
         {wide_period} ms whole, {narrow_period} ms shared",
        share.len(),
        whole.len()
    );
}
