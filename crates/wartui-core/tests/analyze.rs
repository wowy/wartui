//! Loss figures read back from a capture the store wrote.

use std::time::Duration;

use rusqlite::Connection;
use wartui_core::analyze::{BridgeLoss, HeartbeatWindow, HostLoss, losses};
use wartui_core::record::{BatchGap, BridgeStatusSeen, Heartbeat, HostStatus, Record};
use wartui_core::store::{CaptureInfo, Store, StoreConfig, open_readonly};
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelPool;

const NODE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
const OTHER: Mac = [0x02, 0x00, 0x5E, 0x10, 0x1C, 0x5A];
const EPOCH_MS: i64 = 1_777_642_477_000;

/// A fresh capture holding `records`.
fn capture(records: Vec<Record>) -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(10);
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");
    assert_eq!(store.submit(records), 0, "nothing should have been dropped");
    store.close();
    let conn = open_readonly(&path).expect("reopening read-only");
    (dir, conn)
}

fn status(at_s: i64, rx_count: u32, dropped_tx: u32, uptime_ms: u32) -> Record {
    read_status(at_s, rx_count, dropped_tx, uptime_ms, 0)
}

/// A status reply beside the frames the host had read when it arrived.
fn read_status(at_s: i64, rx_count: u32, dropped_tx: u32, uptime_ms: u32, host: u64) -> Record {
    Record::BridgeStatus(BridgeStatusSeen {
        rx_at_ms: EPOCH_MS + at_s * 1000,
        peer_count: 2,
        rx_count,
        dropped_tx,
        uptime_ms,
        host_frames: host,
    })
}

/// A live heartbeat.
fn beat(node: Mac, at_ms: i64, counter: u32, seq: u16, wifi: u16, ble: u16) -> Record {
    Record::Heartbeat(Heartbeat {
        node_mac: node,
        rx_at_ms: EPOCH_MS + at_ms,
        counter,
        epoch: 3,
        link_rssi: Some(-40),
        wifi_refused: wifi,
        ble_refused: ble,
        beat: seq,
        unsent: 0,
        dwell: 0,
        prev_dwell: 0,
        live: true,
    })
}

/// A heartbeat replayed from the bridge's backlog.
fn replayed(node: Mac, at_ms: i64, counter: u32, seq: u16) -> Record {
    let Record::Heartbeat(hb) = beat(node, at_ms, counter, seq, 0, 0) else { unreachable!() };
    Record::Heartbeat(Heartbeat { live: false, ..hb })
}

/// `record`, a heartbeat, carrying `unsent` heartbeats the node's radio could not send since boot.
fn unsent(record: Record, unsent: u8) -> Record {
    let Record::Heartbeat(hb) = record else { unreachable!() };
    Record::Heartbeat(Heartbeat { unsent, ..hb })
}

fn gap(node: Mac, seq: u16, lost: u16) -> Record {
    Record::BatchGap(BatchGap {
        node_mac: node,
        rx_at_ms: EPOCH_MS,
        after_seq: seq - lost - 1,
        seq,
        lost,
    })
}

/// Assigned heartbeats every 5 s, from `from_s`, every one sent.
fn steady(node: Mac, from_s: i64, count: u32) -> Vec<Record> {
    (0..count)
        .map(|i| {
            let seq = u16::try_from(i + 1).expect("a short run");
            beat(node, (from_s + i64::from(i) * 5) * 1000, i + 1, seq, 0, 0)
        })
        .collect()
}

#[test]
fn analyze_counts_bridge_drops_from_first_row_when_capture_starts_with_drops() {
    // 500 dropped before the capture began are not the capture's.
    let (_dir, conn) = capture(vec![
        status(0, 10_000, 500, 60_000),
        status(5, 10_400, 520, 65_000),
        status(10, 10_900, 530, 70_000),
    ]);
    let loss = losses(&conn).unwrap();
    assert_eq!(
        loss.bridge,
        Some(BridgeLoss { received: 900, dropped: 30, reboots: 0, host_read: 0, usb_lost: 870 })
    );
}

#[test]
fn analyze_adds_whole_value_when_bridge_drops_fall() {
    // The bridge restarted between the second and third reply, so the third's counts
    // are all new.
    let (_dir, conn) = capture(vec![
        status(0, 10_000, 500, 60_000),
        status(5, 10_400, 520, 65_000),
        status(10, 300, 7, 4_000),
    ]);
    let loss = losses(&conn).unwrap();
    assert_eq!(
        loss.bridge,
        Some(BridgeLoss { received: 700, dropped: 27, reboots: 1, host_read: 0, usb_lost: 673 })
    );
}

#[test]
fn analyze_omits_bridge_when_no_status_rows() {
    let (_dir, conn) = capture(steady(NODE, 0, 3));
    assert_eq!(losses(&conn).unwrap().bridge, None);
}

#[test]
fn analyze_sums_batch_gaps_per_node_when_gaps_recorded() {
    let (_dir, conn) = capture(vec![
        gap(NODE, 10, 2),
        gap(NODE, 50, 5),
        gap(OTHER, 20, 1),
        beat(OTHER, 0, 1, 1, 0, 0),
    ]);
    let loss = losses(&conn).unwrap();
    // Sorted by address, and a node with batch gaps but no heartbeat is still listed.
    let summary: Vec<_> =
        loss.nodes.iter().map(|n| (n.mac, n.batches_lost, n.heartbeats)).collect();
    assert_eq!(summary, vec![(OTHER, 1, 1), (NODE, 7, 0)]);
}

#[test]
fn analyze_counts_missed_heartbeats_when_beat_skips() {
    // Beats 3 and 4 never arrived. The counter jumps further, since it counts sweeps.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 1, 0, 0),
        beat(NODE, 5_000, 4, 2, 0, 0),
        beat(NODE, 20_000, 13, 5, 0, 0),
        beat(NODE, 25_000, 16, 6, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (4, 2));
}

#[test]
fn analyze_ignores_duplicate_heartbeat_when_beat_repeats() {
    // A radio retransmit or a replay: the same beat twice is one heartbeat and no loss.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 1, 0, 0),
        beat(NODE, 5_000, 4, 2, 0, 0),
        beat(NODE, 5_010, 4, 2, 0, 0),
        beat(NODE, 10_000, 7, 3, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (3, 0));
}

#[test]
fn analyze_counts_across_wrap_when_beat_wraps() {
    // 65535 then 1: beat 0 was lost.
    let (_dir, conn) =
        capture(vec![beat(NODE, 0, 900, u16::MAX, 0, 0), beat(NODE, 10_000, 903, 1, 0, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (2, 1));
}

#[test]
fn analyze_ignores_gap_when_counter_falls() {
    // The node rebooted during the silence, so the beat restarted and the gap is not loss.
    let (_dir, conn) = capture(vec![beat(NODE, 0, 40, 20, 0, 0), beat(NODE, 60_000, 1, 1, 0, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (2, 0));
}

#[test]
fn analyze_counts_gap_when_sweep_counter_wraps() {
    // A small forward modulo delta is a wrap, not a reboot. Beats 101 and 102 were lost.
    let (_dir, conn) =
        capture(vec![beat(NODE, 0, u32::MAX - 1, 100, 10, 0), beat(NODE, 15_000, 2, 103, 12, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed, node.wifi_refused), (2, 2, 2));
}

#[test]
fn analyze_brackets_windows_when_beats_skip_and_repeat() {
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 100, 80, 0, 0),
        beat(NODE, 5_000, 101, 81, 0, 0),
        beat(NODE, 5_010, 101, 81, 0, 0),
        beat(NODE, 20_000, 104, 84, 0, 0),
        beat(NODE, 20_010, 104, 84, 0, 0),
        beat(NODE, 30_000, 106, 86, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (4, 3));
    assert_eq!(
        node.heartbeat_windows,
        vec![
            HeartbeatWindow {
                start_id: 3,
                end_id: 4,
                start_rx_at_ms: EPOCH_MS + 5_010,
                end_rx_at_ms: EPOCH_MS + 20_000,
                missed: 2,
                unsent: 0,
                restarted: false,
            },
            HeartbeatWindow {
                start_id: 5,
                end_id: 6,
                start_rx_at_ms: EPOCH_MS + 20_010,
                end_rx_at_ms: EPOCH_MS + 30_000,
                missed: 1,
                unsent: 0,
                restarted: false,
            },
        ]
    );
}

#[test]
fn analyze_marks_windows_when_restart_beats_are_missing() {
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 40, 20, 0, 0),
        beat(NODE, 60_000, 1, 12, 0, 0),
        beat(NODE, 65_000, 2, 13, 0, 0),
        beat(NODE, 120_000, 10, 3, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (4, 13));
    assert_eq!(node.heartbeat_windows.len(), 2);
    assert_eq!(
        node.heartbeat_windows[0],
        HeartbeatWindow {
            start_id: 1,
            end_id: 2,
            start_rx_at_ms: EPOCH_MS,
            end_rx_at_ms: EPOCH_MS + 60_000,
            missed: 11,
            unsent: 0,
            restarted: true,
        }
    );
    assert_eq!(node.heartbeat_windows[1].missed, 2);
    assert!(node.heartbeat_windows[1].restarted);
}

#[test]
fn analyze_preserves_window_arrival_order_when_clock_reverses() {
    let (_dir, conn) =
        capture(vec![beat(NODE, 5_000, 1, 1, 0, 0), beat(NODE, -25_000, 4, 4, 0, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!(
        node.heartbeat_windows,
        vec![HeartbeatWindow {
            start_id: 1,
            end_id: 2,
            start_rx_at_ms: EPOCH_MS + 5_000,
            end_rx_at_ms: EPOCH_MS - 25_000,
            missed: 2,
            unsent: 0,
            restarted: false,
        }]
    );
}

#[test]
fn analyze_excludes_windows_when_first_beats_and_handoff_are_baselines() {
    let (_dir, conn) = capture(vec![
        replayed(NODE, 0, 30, 17),
        beat(NODE, 1_000, 240, 140, 0, 0),
        beat(NODE, 21_000, 244, 144, 0, 0),
        replayed(OTHER, 0, 40, 20),
        beat(OTHER, 60_000, 1, 12, 0, 0),
    ]);
    let loss = losses(&conn).unwrap();
    assert!(loss.nodes[0].heartbeat_windows.is_empty());
    let node = &loss.nodes[1];
    assert_eq!(node.heartbeats_missed, 3);
    assert_eq!(
        node.heartbeat_windows,
        vec![HeartbeatWindow {
            start_id: 2,
            end_id: 3,
            start_rx_at_ms: EPOCH_MS + 1_000,
            end_rx_at_ms: EPOCH_MS + 21_000,
            missed: 3,
            unsent: 0,
            restarted: false,
        }]
    );
}

#[test]
fn analyze_counts_windows_when_both_counters_wrap() {
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, u32::MAX - 1, u16::MAX, 0, 0),
        beat(NODE, 10_000, 2, 1, 0, 0),
        beat(NODE, 15_000, 3, 2, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (3, 1));
    assert_eq!(node.heartbeat_windows.len(), 1);
    assert_eq!(node.heartbeat_windows[0].missed, 1);
    assert!(!node.heartbeat_windows[0].restarted);
}

#[test]
fn analyze_classifies_restart_when_modulo_delta_reaches_half_range() {
    for (counter, seq, missed, restarted) in [
        (1, 10 + 0x7fff, 0x7ffe, false),
        (1, 10 + 0x8000, 10 + 0x8000 - 1, true),
        (0x7fff_ffff, 12, 1, false),
        (0x8000_0000, 12, 11, true),
    ] {
        let (_dir, conn) =
            capture(vec![beat(NODE, 0, 0, 10, 0, 0), beat(NODE, 10_000, counter, seq, 0, 0)]);
        let node = &losses(&conn).unwrap().nodes[0];
        assert_eq!(node.heartbeats_missed, missed);
        assert_eq!(node.heartbeat_windows.len(), 1);
        assert_eq!(node.heartbeat_windows[0].missed, missed);
        assert_eq!(node.heartbeat_windows[0].restarted, restarted);
    }
}

#[test]
fn analyze_keeps_windows_per_node_when_arrivals_are_interleaved() {
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 1, 0, 0),
        beat(OTHER, 1_000, 100, 100, 0, 0),
        beat(NODE, 10_000, 3, 3, 0, 0),
        beat(OTHER, 21_000, 104, 104, 0, 0),
        beat(NODE, 15_000, 1, 1, 0, 0),
    ]);
    let loss = losses(&conn).unwrap();
    assert_eq!(loss.nodes[0].heartbeats_missed, 3);
    assert_eq!(loss.nodes[1].heartbeats_missed, 1);
    assert_eq!(loss.nodes[0].heartbeat_windows[0].start_id, 2);
    assert_eq!(loss.nodes[0].heartbeat_windows[0].end_id, 4);
    assert_eq!(loss.nodes[1].heartbeat_windows[0].start_id, 1);
    assert_eq!(loss.nodes[1].heartbeat_windows[0].end_id, 3);
    for node in &loss.nodes {
        assert_eq!(
            node.heartbeats_missed,
            node.heartbeat_windows.iter().map(|window| window.missed).sum::<u64>()
        );
    }
}

#[test]
fn analyze_rebases_refusals_when_node_reboots() {
    let (_dir, conn) = capture(vec![
        // 100 refused before the capture began: a baseline.
        beat(NODE, 0, 10, 5, 100, 4),
        beat(NODE, 5_000, 11, 6, 130, 6),
        // Rebooted: the counts restarted, so all of each is new.
        beat(NODE, 10_000, 1, 1, 20, 1),
        beat(NODE, 11_000, 2, 2, 25, 1),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.wifi_refused, node.ble_refused), (30 + 20 + 5, 2 + 1));
}

#[test]
fn analyze_walks_arrival_order_when_clock_steps_back() {
    // The host clock steps back 30 s after the second heartbeat. Arrival order still has the
    // beats rising by one, so nothing was missed and nothing restarted.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 1, 10, 0),
        beat(NODE, 5_000, 2, 2, 12, 0),
        beat(NODE, -25_000, 3, 3, 15, 0),
        beat(NODE, -20_000, 4, 4, 16, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed, node.wifi_refused), (4, 0, 6));
}

#[test]
fn analyze_reads_falling_beat_as_reboot_when_counter_rises() {
    // The node rebooted out of range and came back with a higher sweep counter. The falling
    // beat is the restart, so the refusal counts are all new and beat 1 follows no loss.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 10, 40, 100, 0),
        beat(NODE, 5_000, 11, 41, 110, 0),
        beat(NODE, 60_000, 50, 1, 30, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed, node.wifi_refused), (3, 0, 10 + 30));
}

#[test]
fn analyze_counts_beats_lost_since_boot_when_first_heard_late() {
    // Rebooted, and beats 1 to 11 since boot never arrived.
    let (_dir, conn) = capture(vec![beat(NODE, 0, 40, 20, 0, 0), beat(NODE, 60_000, 1, 12, 0, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (2, 11));
}

#[test]
fn analyze_ignores_beat_gap_when_previous_heartbeat_replayed() {
    // Beat 17 was replayed from the bridge's backlog; 140 is the first live one. The beats
    // between were sent while no host was reading, which the bridge's drop count covers.
    let (_dir, conn) = capture(vec![
        replayed(NODE, 0, 30, 17),
        beat(NODE, 1_000, 240, 140, 0, 0),
        beat(NODE, 6_000, 241, 141, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (3, 0));
}

#[test]
fn analyze_counts_beat_gap_when_previous_heartbeat_live() {
    let (_dir, conn) = capture(vec![
        replayed(NODE, 0, 30, 17),
        beat(NODE, 1_000, 240, 140, 0, 0),
        beat(NODE, 21_000, 244, 144, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (3, 3));
}

#[test]
fn analyze_ignores_beats_lost_since_boot_when_previous_heartbeat_replayed() {
    // The node rebooted while no host was reading; beats 1 to 11 fall in that time.
    let (_dir, conn) = capture(vec![replayed(NODE, 0, 40, 20), beat(NODE, 60_000, 1, 12, 0, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.heartbeats_missed), (2, 0));
}

#[test]
fn analyze_splits_missed_heartbeats_into_unsent_when_unsent_advances_across_gap() {
    // Beats 3 to 5 are missing, and the radio could not send two of them.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 1, 0, 0),
        beat(NODE, 5_000, 2, 2, 0, 0),
        unsent(beat(NODE, 30_000, 7, 6, 0, 0), 2),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats_missed, node.heartbeats_unsent), (3, 2));
    assert_eq!(node.heartbeat_windows[0].unsent, 2);
}

#[test]
fn analyze_counts_unsent_since_boot_when_node_restarts() {
    // Rebooted: beats 1 to 11 since boot are missing, and the radio could not send 3 of them.
    let (_dir, conn) = capture(vec![
        unsent(beat(NODE, 0, 40, 20, 0, 0), 5),
        unsent(beat(NODE, 60_000, 1, 12, 0, 0), 3),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats_missed, node.heartbeats_unsent), (11, 3));
    assert!(node.heartbeat_windows[0].restarted);
    assert_eq!(node.heartbeat_windows[0].unsent, 3);
}

#[test]
fn analyze_ignores_unsent_when_gap_follows_replayed_heartbeat() {
    // The 50 unsent between the replayed beat and the first live one fall in time no host was
    // reading. Only the one unsent between live beats 140 and 144 counts.
    let (_dir, conn) = capture(vec![
        replayed(NODE, 0, 30, 17),
        unsent(beat(NODE, 1_000, 240, 140, 0, 0), 50),
        unsent(beat(NODE, 21_000, 244, 144, 0, 0), 51),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats_missed, node.heartbeats_unsent), (3, 1));
}

#[test]
fn analyze_caps_unsent_at_missed_when_count_exceeds_gap() {
    // Two failed sends across one missed beat cannot come from honest rows; the count caps at
    // the gap.
    let (_dir, conn) =
        capture(vec![beat(NODE, 0, 1, 1, 0, 0), unsent(beat(NODE, 10_000, 3, 3, 0, 0), 2)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats_missed, node.heartbeats_unsent), (1, 1));
    assert_eq!(node.heartbeat_windows[0].unsent, 1);
}

#[test]
fn analyze_counts_unsent_across_wrap_when_count_passes_255() {
    // 250 to 3 is nine failed sends, not a restart.
    let (_dir, conn) = capture(vec![
        unsent(beat(NODE, 0, 10, 10, 0, 0), 250),
        unsent(beat(NODE, 60_000, 22, 22, 0, 0), 3),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats_missed, node.heartbeats_unsent), (11, 9));
    assert!(!node.heartbeat_windows[0].restarted);
}

/// A host row with every count at `n` and the given peaks and health.
fn host_row(
    n: u64,
    lag_us: u64,
    throttled: Option<u32>,
    temp: Option<i32>,
    battery_mv: Option<i32>,
) -> Record {
    Record::HostStatus(HostStatus {
        at_ms: EPOCH_MS,
        frames: n,
        duplicate_batches: n,
        garbled: n,
        undecodable: n,
        incompatible: n,
        foreign_fleet: n,
        foreign_admin: n,
        admin_windows_missed: n,
        lag_peak_us: lag_us,
        store_written: n,
        store_dropped: n,
        store_queue_peak: n,
        store_commit_peak_us: n * 10,
        throttled,
        soc_temp_mc: temp,
        battery_mv,
        battery_ma: battery_mv.map(|_| 21),
    })
}

#[test]
fn analyze_sums_host_read_from_first_row_when_status_rows_carry_host_frames() {
    // 30 frames the bridge received never reached the host; a bridge restart leaves the
    // host's own count rising.
    let (_dir, conn) = capture(vec![
        read_status(0, 10_000, 0, 60_000, 4_000),
        read_status(5, 10_400, 0, 65_000, 4_390),
        read_status(10, 300, 0, 4_000, 4_670),
    ]);
    let loss = losses(&conn).unwrap();
    assert_eq!(
        loss.bridge,
        Some(BridgeLoss { received: 700, dropped: 0, reboots: 1, host_read: 670, usb_lost: 30 })
    );
}

#[test]
fn analyze_excludes_bridge_drops_from_usb_lost_when_bridge_dropped_frames() {
    // 1,000 received, 10 of them evicted from the bridge's outbox and never sent, and 985
    // read: 5 were lost on USB.
    let (_dir, conn) = capture(vec![
        read_status(0, 10_000, 100, 60_000, 9_000),
        read_status(5, 10_400, 104, 65_000, 9_394),
        read_status(10, 11_000, 110, 70_000, 9_985),
    ]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!((bridge.received, bridge.dropped, bridge.host_read), (1_000, 10, 985));
    assert_eq!(bridge.usb_lost, 5);
}

#[test]
fn analyze_saturates_usb_lost_when_baseline_taken_mid_backlog() {
    // The first reply overtook 24 frames still queued in the bridge, so the host had read
    // 24 fewer than `rx_count` says and reads them after the baseline: over the capture it
    // reads more than the bridge received.
    let (_dir, conn) = capture(vec![
        read_status(0, 10_000, 0, 60_000, 9_976),
        read_status(5, 10_400, 0, 65_000, 10_400),
    ]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!((bridge.received, bridge.host_read), (400, 424));
    assert_eq!(bridge.usb_lost, 0);
}

#[test]
fn analyze_reads_host_deltas_and_peaks_when_host_rows_present() {
    let (_dir, conn) = capture(vec![
        host_row(10, 0, Some(0x0000), Some(55_000), Some(4_185)),
        host_row(15, 150_000, Some(0x50005), Some(71_200), Some(3_620)),
        host_row(25, 99_999, Some(0x50002), None, None),
        host_row(40, 100_000, Some(0x50000), Some(60_000), Some(3_900)),
    ]);
    let host = losses(&conn).unwrap().host.expect("host rows");
    assert_eq!(
        host,
        HostLoss {
            duplicates: 30,
            garbled: 30,
            undecodable: 30,
            foreign: 90,
            admin_windows_missed: 30,
            store_dropped: 30,
            lag_over_100ms: 2,
            lag_peak_us: 150_000,
            commit_peak_us: 400,
            queue_peak: 40,
            under_voltage: Some(1),
            throttled: Some(2),
            since_boot: Some(0b0101),
            temp_max_mc: Some(71_200),
            battery_min_mv: Some(3_620),
        }
    );
}

#[test]
fn analyze_reads_no_pi_figures_when_health_null() {
    let (_dir, conn) =
        capture(vec![host_row(1, 0, None, None, None), host_row(2, 0, None, None, None)]);
    let host = losses(&conn).unwrap().host.expect("host rows");
    assert_eq!(
        (host.under_voltage, host.throttled, host.since_boot, host.temp_max_mc),
        (None, None, None, None)
    );
    assert_eq!(host.battery_min_mv, None);
}

#[test]
fn analyze_omits_host_when_no_host_rows() {
    let (_dir, conn) = capture(steady(NODE, 0, 3));
    assert_eq!(losses(&conn).unwrap().host, None);
}
