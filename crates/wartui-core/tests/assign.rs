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
//!
//! The period is a median over the last [`BEAT_WINDOW`] gaps and is not reset
//! when a node adopts a new range, so each measurement here waits out a whole
//! window of beats *counted from the assignment it is measuring*. Waiting for an
//! absolute number of heartbeats instead would measure whatever mixture of ranges
//! the node happened to have beaten at — including the idle beat it parks on
//! before it is assigned anything, which at this speed is shorter than either
//! sweep and would read as a node scanning faster than it can.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{BEAT_WINDOW, Command, EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::record::AdminOutcome;
use wartui_core::runtime::{drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::plan::{CHANNEL_DWELL_MS, ChannelPool, ChannelSet, IndexRun, NUM_SCAN_CHANNELS};

/// How much faster than real time the fake fleet runs.
///
/// Named because the periods being measured are derived from it: at 60 the
/// node's idle beat is 17 ms, its sweep of the whole table 88 ms and its sweep
/// of one channel 7 ms. All three are far shorter than the 250 ms
/// [`wartui_core::runtime::TICK`] the snapshot is republished on, which is why
/// nothing below infers anything from *when* a snapshot was seen.
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
    let link = SimTransport::new(SimConfig {
        node_count: 1,
        speed: f64::from(SPEED),
        ..Default::default()
    })
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

    // The wide baseline has to be asked for. A wartui node that has never
    // heard a core parks on the control channel and collects nothing, rather
    // than defaulting to all forty channels the way the vendor firmware does
    // (`src/WiFiOps.cpp:77-80`) — so an unassigned node's heartbeat period says
    // nothing about sweeping at all.
    let joined = until(&snapshot_rx, "the node to turn up", |s| {
        s.nodes.first().is_some_and(|n| n.state.heartbeats >= 1)
    })
    .await;
    let mac = joined.nodes[0].state.mac;
    assert!(joined.nodes[0].state.confirmed.is_none(), "nothing has assigned it yet");

    command_tx
        .send(Command::Assign { mac, channels: ChannelPool::All.channels() })
        .await
        .expect("the engine is listening");
    let wide_acked = until(&snapshot_rx, "the whole table to be acknowledged", |s| {
        s.nodes.first().is_some_and(|n| {
            n.state.confirmed.is_some_and(|c| c.channels == ChannelPool::All.channels())
        })
    })
    .await;

    // A whole window of sweeps, counted from the beat that opened the admin
    // window rather than from the start of the capture. Every beat before this
    // one was an idle beat — a wartui node parks until it is told what to scan —
    // and the period is a median over the last [`BEAT_WINDOW`] gaps, so a window
    // with any of them left in it reports the parked node's 17 ms rather than the
    // sweep. How many there were is not a fixed number to skip: nothing goes on
    // the air until a heartbeat opens a window, so any delay in handling the
    // operator's command adds another, and the snapshot is only republished every
    // 250 ms in any case.
    let window = u64::try_from(BEAT_WINDOW).expect("a five-deep window fits in a u64");
    let idle_beats = wide_acked.nodes[0].state.heartbeats;
    let wide = until(&snapshot_rx, "a full window of sweeps of the whole table", |s| {
        s.nodes.first().is_some_and(|n| n.state.heartbeats >= idle_beats + window)
    })
    .await;
    let wide_period = wide.nodes[0].state.beat_period_ms().expect("a measured period");
    // Half a sweep of the whole table, which is still several times the idle
    // beat. Asserting it here says which number was wrong when it is wrong,
    // rather than leaving the ratio below to fail for either reason.
    let half_a_sweep = u32::from(NUM_SCAN_CHANNELS) * CHANNEL_DWELL_MS / SPEED / 2;
    assert!(
        wide_period > half_a_sweep,
        "the whole table should take at least {half_a_sweep} ms, not the idle beat: {wide_period} ms"
    );

    command_tx
        .send(Command::Assign { mac, channels: ChannelSet::from_run(IndexRun::new(5, 5)) })
        .await
        .expect("the engine is listening");

    // Nothing goes out until the node's next heartbeat opens its admin window,
    // and the node adopts the channels only because the epoch differs from the
    // 0 it booted with — the `!=` at `src/WiFiOps.cpp:1198`.
    let acked = until(&snapshot_rx, "the narrow assignment to be acknowledged", |s| {
        s.nodes.first().is_some_and(|n| n.state.confirmed.is_some_and(|c| c.channels.len() == 1))
    })
    .await;
    let node = &acked.nodes[0];
    assert_eq!(
        node.state.confirmed.expect("confirmed").channels,
        ChannelSet::from_run(IndexRun::new(5, 5))
    );
    assert!(!node.state.dirty, "acknowledged, so nothing is owed");
    assert_eq!(acked.counters.admin_acked, 2, "the wide assignment, then the narrow one");
    assert_eq!(acked.counters.admin_failed, 0);

    // A full window again, so not one gap from the wide range is left in the
    // median. This is the whole milestone: one channel instead of forty, so about
    // a fortieth of the sweep, observable without any access to the node beyond
    // its radio.
    let beats_before = node.state.heartbeats;
    let narrow = until(&snapshot_rx, "a full window of sweeps at the new range", |s| {
        s.nodes.first().is_some_and(|n| n.state.heartbeats >= beats_before + window)
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
    let (outcome, channels, ble, latency): (String, i64, bool, Option<i64>) = conn
        .query_row(
            "SELECT outcome, channels, ble, latency_us FROM assignment ORDER BY id DESC",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("an assignment row");
    assert_eq!(outcome, AdminOutcome::Acked.as_str());
    // Stored as the mask that went on the wire, so the row says what the node
    // was told rather than an interpretation of it.
    assert_eq!(
        ChannelSet::from_bits(u64::try_from(channels).expect("a 40-bit mask")),
        ChannelSet::from_run(IndexRun::new(5, 5))
    );
    assert!(!ble);
    // The number that settles whether a bridge this dumb can hit a 300 ms
    // window. Measured on the bridge's own clock, from the heartbeat that
    // opened the window to the transmit callback.
    assert!(latency.is_some(), "and carries the latency the whole design turns on");
}
