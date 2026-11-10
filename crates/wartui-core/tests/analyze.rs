//! Loss and arrival-rate figures read back from a capture the store wrote.

use std::collections::BTreeMap;
use std::time::Duration;

use rusqlite::Connection;
use wartui_core::analyze::{
    BridgeLoss, HostLoss, NodePeak, Peak, PeakSummary, QueueLoss, RadioLinkLoss, losses, peaks,
};
use wartui_core::position::Fix;
use wartui_core::record::{
    BatchGap, BridgeSeen, BridgeStatusSeen, Heartbeat, HostStatus, Observation, Record,
};
use wartui_core::store::{CaptureInfo, Store, StoreConfig, open_readonly};
use wartui_proto::bus::{NodeBoard, RecordKind, Slot};
use wartui_proto::link::{QueueId, QueueStat, QueueStats, RadioLinkStats};
use wartui_proto::mac::Mac;
use wartui_proto::reset::ResetCause;

const NODE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
const OTHER: Mac = [0x02, 0x00, 0x5E, 0x10, 0x1C, 0x5A];
const EPOCH_MS: i64 = 1_777_642_477_000;
const SLOT: Slot = Slot::new(0, 0).unwrap();

/// A fresh capture holding `records`.
fn capture(records: Vec<Record>) -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.batch_rows = 8;
    config.batch_interval = Duration::from_millis(10);
    let info = CaptureInfo { notes: None, simulated: false };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");
    assert_eq!(store.submit(records), 0, "nothing should have been dropped");
    store.close();
    let conn = open_readonly(&path).expect("reopening read-only");
    (dir, conn)
}

/// `peaks` where the figures are the point and no node's slot is.
fn peak_figures(conn: &Connection) -> PeakSummary {
    peaks(conn, &BTreeMap::new()).expect("reading peaks")
}

fn status(at_s: i64, rx_count: u32, dropped_tx: u32, uptime_ms: u32) -> Record {
    read_status(at_s, rx_count, dropped_tx, uptime_ms, 0)
}

/// A status reply beside the node records the host had read when it arrived.
fn read_status(at_s: i64, rx_count: u32, dropped_tx: u32, uptime_ms: u32, host: u64) -> Record {
    Record::BridgeStatus(BridgeStatusSeen {
        rx_at_ms: EPOCH_MS + at_s * 1000,
        rx_count,
        dropped_tx,
        uptime_ms,
        host_frames: host,
        rx_oversize: 0,
        queues: QueueStats::new(),
        radio_link: RadioLinkStats::default(),
        host_uart_errors: 0,
        fan: None,
        slots_present: 0,
    })
}

/// A status reply carrying one queue's figures.
fn queue_status(uptime_ms: u32, queue: QueueId, high_water: u32, dropped: u32) -> Record {
    let Record::BridgeStatus(seen) = status(0, 0, 0, uptime_ms) else { unreachable!() };
    let queues = [QueueStat { queue, capacity: 1_000, high_water, dropped }].into_iter().collect();
    Record::BridgeStatus(BridgeStatusSeen { queues, ..seen })
}

/// A status reply carrying several queues' drops, with `dropped_tx` summed as the bridge sums it.
fn queues_status(uptime_ms: u32, drops: &[(QueueId, u32)]) -> Record {
    let dropped_tx = drops.iter().filter(|(q, _)| q.in_dropped_tx()).map(|(_, d)| d).sum();
    let Record::BridgeStatus(seen) = status(0, 0, dropped_tx, uptime_ms) else { unreachable!() };
    let queues = drops
        .iter()
        .map(|&(queue, dropped)| QueueStat { queue, capacity: 1_000, high_water: 10, dropped })
        .collect();
    Record::BridgeStatus(BridgeStatusSeen { queues, ..seen })
}

/// A status reply carrying link counters: `packets`, then `n`, `2n` … `8n` in field order, so
/// no two counters match.
fn radio_link_status(uptime_ms: u32, packets: u32, n: u32) -> Record {
    radio_link_status_split(uptime_ms, packets, n, n)
}

/// As [`radio_link_status`], with the P4's counters on `p4` and the C6's `radio_*` on `c6`.
fn radio_link_status_split(uptime_ms: u32, packets: u32, p4: u32, c6: u32) -> Record {
    let Record::BridgeStatus(seen) = status(0, 0, 0, uptime_ms) else { unreachable!() };
    let radio_link = RadioLinkStats {
        packets,
        crc_fail: p4,
        bad_magic: 2 * p4,
        seq_gap: 3 * p4,
        uart_errors: 4 * p4,
        radio_crc_fail: 5 * c6,
        radio_bad_magic: 6 * c6,
        radio_seq_gap: 7 * c6,
        radio_uart_errors: 8 * c6,
    };
    Record::BridgeStatus(BridgeStatusSeen { radio_link, ..seen })
}

/// A status reply carrying a count of oversize node records dropped.
fn oversize_status(at_s: i64, uptime_ms: u32, rx_oversize: u32) -> Record {
    let Record::BridgeStatus(seen) = status(at_s, 0, 0, uptime_ms) else { unreachable!() };
    Record::BridgeStatus(BridgeStatusSeen { rx_oversize, ..seen })
}

/// A live heartbeat.
fn beat(node: Mac, at_ms: i64, counter: u32, wifi: u16, ble: u16) -> Record {
    Record::Heartbeat(Heartbeat {
        node_mac: node,
        rx_at_ms: EPOCH_MS + at_ms,
        counter,
        epoch: 3,
        slot: SLOT,
        wifi_refused: wifi,
        ble_refused: ble,
        reset_cause: ResetCause::PowerOn,
        vdd_mv: None,
        die_temp_c: None,
        heap_free: 40_000,
        live: true,
    })
}

/// A heartbeat replayed from the bridge's backlog.
fn replayed(node: Mac, at_ms: i64, counter: u32) -> Record {
    let Record::Heartbeat(hb) = beat(node, at_ms, counter, 0, 0) else { unreachable!() };
    Record::Heartbeat(Heartbeat { live: false, ..hb })
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

/// Assigned heartbeats every 5 s, from `from_s`.
fn steady(node: Mac, from_s: i64, count: u32) -> Vec<Record> {
    (0..count).map(|i| beat(node, (from_s + i64::from(i) * 5) * 1000, i + 1, 0, 0)).collect()
}

/// A sighting `at_ms` after the capture's start, with no position.
fn sighting(node: Mac, kind: RecordKind, at_ms: i64) -> Record {
    Record::Observation(Observation {
        node_mac: node,
        rx_at_ms: EPOCH_MS + at_ms,
        slot: SLOT,
        bssid: [0xAA; 6],
        ssid: b"example".to_vec(),
        security: "[WPA2_PSK]".to_owned(),
        channel: if kind == RecordKind::Wifi { 6 } else { 0 },
        rssi: -60,
        kind,
        rcoi: None,
        mfgr_id: None,
        fix: Fix::none(),
        raw_body: b"raw".to_vec(),
    })
}

/// `count` sightings arriving together.
fn burst(node: Mac, kind: RecordKind, at_ms: i64, count: usize) -> Vec<Record> {
    (0..count).map(|_| sighting(node, kind, at_ms)).collect()
}

/// The total peaks of Wi-Fi sightings from `NODE` at each offset.
fn total_peak(at_ms: &[i64]) -> Peak {
    let (_dir, conn) =
        capture(at_ms.iter().map(|&at| sighting(NODE, RecordKind::Wifi, at)).collect());
    peak_figures(&conn).total
}

#[test]
fn analyze_restarts_peak_windows_when_clock_steps_back() {
    let mut records = burst(NODE, RecordKind::Wifi, 0, 3);
    records.extend(burst(NODE, RecordKind::Wifi, 5_000, 3));
    records.extend(burst(NODE, RecordKind::Wifi, 4_800, 3));
    let (_dir, conn) = capture(records);
    let summary = peak_figures(&conn);
    // Sorted by `rx_at`, the step would merge into {6, 9}: a peak no link carried.
    assert_eq!(summary.total, Peak { per_second: 3, per_minute: 6 });
    assert_eq!(summary.clock_reversals, 1);
    assert_eq!(summary.sightings, 9);
}

#[test]
fn analyze_separates_peak_windows_when_sightings_are_exactly_one_span_apart() {
    assert_eq!(total_peak(&[0, 1_000, 2_000]).per_second, 1);
    assert_eq!(total_peak(&[0, 999]).per_second, 2);
    assert_eq!(total_peak(&[0, 60_000]).per_minute, 1);
    assert_eq!(total_peak(&[0, 59_999]).per_minute, 2);
}

#[test]
fn analyze_reports_burst_as_peak_when_sightings_arrive_together() {
    let mut records = burst(NODE, RecordKind::Wifi, 0, 5);
    records.extend(burst(NODE, RecordKind::Wifi, 10_000, 2));
    let (_dir, conn) = capture(records);
    let summary = peak_figures(&conn);
    assert_eq!(summary.total, Peak { per_second: 5, per_minute: 7 });
    assert_eq!(summary.sightings, 7);
}

#[test]
fn analyze_joins_peak_burst_when_it_straddles_a_clock_second() {
    let mut records = burst(NODE, RecordKind::Wifi, 900, 3);
    records.extend(burst(NODE, RecordKind::Wifi, 1_100, 4));
    let (_dir, conn) = capture(records);
    assert_eq!(peak_figures(&conn).total.per_second, 7);
}

#[test]
fn analyze_reports_minute_peak_above_second_when_load_is_sustained() {
    let records = (0..=7).flat_map(|i| burst(NODE, RecordKind::Wifi, i * 10_000, 2)).collect();
    let (_dir, conn) = capture(records);
    assert_eq!(peak_figures(&conn).total, Peak { per_second: 2, per_minute: 12 });
}

#[test]
fn analyze_reports_peak_per_node_when_nodes_interleave() {
    let records = vec![
        sighting(NODE, RecordKind::Wifi, 0),
        sighting(OTHER, RecordKind::Wifi, 0),
        sighting(NODE, RecordKind::Wifi, 0),
        sighting(OTHER, RecordKind::Wifi, 0),
        sighting(NODE, RecordKind::Wifi, 0),
    ];
    let (_dir, conn) = capture(records);
    let summary = peak_figures(&conn);
    assert_eq!(summary.total, Peak { per_second: 5, per_minute: 5 });
    assert_eq!(
        summary.nodes,
        vec![
            NodePeak { mac: OTHER, slot: None, peak: Peak { per_second: 2, per_minute: 2 } },
            NodePeak { mac: NODE, slot: None, peak: Peak { per_second: 3, per_minute: 3 } },
        ]
    );
}

#[test]
fn analyze_reports_peak_per_kind_when_wifi_and_ble_arrive() {
    let mut records = burst(NODE, RecordKind::Wifi, 0, 3);
    records.extend(burst(NODE, RecordKind::Ble, 0, 2));
    let (_dir, conn) = capture(records);
    let summary = peak_figures(&conn);
    assert_eq!(summary.wifi, Peak { per_second: 3, per_minute: 3 });
    assert_eq!(summary.ble, Peak { per_second: 2, per_minute: 2 });
    assert_eq!(summary.total, Peak { per_second: 5, per_minute: 5 });
}

#[test]
fn analyze_counts_peak_rows_when_export_leaves_them_out() {
    // An all-zero BSSID is no address an export writes, but it still crossed the link.
    let records = burst(NODE, RecordKind::Wifi, 0, 2)
        .into_iter()
        .map(|record| {
            let Record::Observation(obs) = record else { unreachable!() };
            Record::Observation(Observation { bssid: [0; 6], ..obs })
        })
        .collect();
    let (_dir, conn) = capture(records);
    assert_eq!(peak_figures(&conn).total.per_second, 2);
}

#[test]
fn analyze_counts_peak_in_total_and_node_only_when_kind_is_unknown() {
    let (dir, conn) = capture(vec![sighting(NODE, RecordKind::Wifi, 0)]);
    drop(conn);
    // No build writes this kind, so only an edited file holds one.
    let path = dir.path().join("wartui.db");
    let rw = Connection::open(&path).expect("opening for writing");
    rw.execute(
        "INSERT INTO observation (node_mac, rx_at, slot, bssid, ssid, security, channel, rssi,
                                  kind, rcoi, mfgr_id, lat, lon, alt, accuracy, pos_source, pos_at,
                                  raw_body)
         SELECT node_mac, rx_at, slot, bssid, ssid, security, channel, rssi, 'zigbee', rcoi,
                mfgr_id, lat, lon, alt, accuracy, pos_source, pos_at, raw_body
         FROM observation",
        [],
    )
    .expect("inserting the foreign row");
    drop(rw);
    let conn = open_readonly(&path).expect("reopening read-only");
    let summary = peak_figures(&conn);
    assert_eq!(summary.total, Peak { per_second: 2, per_minute: 2 });
    assert_eq!(summary.wifi, Peak { per_second: 1, per_minute: 1 });
    assert_eq!(summary.ble, Peak::default());
    assert_eq!(summary.nodes, vec![NodePeak { mac: NODE, slot: None, peak: summary.total }]);
}

#[test]
fn analyze_reports_no_peaks_when_capture_holds_no_sightings() {
    let (_dir, conn) = capture(vec![beat(NODE, 0, 1, 0, 0)]);
    assert_eq!(peak_figures(&conn), PeakSummary::default());
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
        Some(BridgeLoss {
            received: 900,
            dropped: 30,
            reboots: 0,
            host_read: 0,
            usb_lost: 870,
            oversize: 0,
            queues: vec![],
            radio_link: RadioLinkLoss::default(),
            host_uart_errors: 0,
        })
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
        Some(BridgeLoss {
            received: 700,
            dropped: 27,
            reboots: 1,
            host_read: 0,
            usb_lost: 673,
            oversize: 0,
            queues: vec![],
            radio_link: RadioLinkLoss::default(),
            host_uart_errors: 0,
        })
    );
}

#[test]
fn analyze_sums_oversize_from_first_row_when_bridge_restarts() {
    // 5 before the capture are a baseline; the restart's 4 are all new.
    let (_dir, conn) = capture(vec![
        oversize_status(0, 60_000, 5),
        oversize_status(5, 65_000, 12),
        oversize_status(10, 4_000, 4),
    ]);
    let loss = losses(&conn).unwrap();
    assert_eq!(loss.bridge.map(|bridge| bridge.oversize), Some(11));
}

#[test]
fn analyze_reports_queue_peak_when_rows_carry_queues() {
    // The peak is the largest high-water any row carried, in the order first reported.
    let (_dir, conn) = capture(vec![
        queue_status(60_000, QueueId::HostBulk, 300, 0),
        queue_status(65_000, QueueId::NodeDown, 20, 0),
        queue_status(70_000, QueueId::HostBulk, 700, 0),
        queue_status(75_000, QueueId::HostBulk, 400, 0),
    ]);
    let queues = losses(&conn).unwrap().bridge.unwrap().queues;
    assert_eq!(
        queues,
        [
            QueueLoss { queue: QueueId::HostBulk, capacity: 1_000, peak: 700, dropped: 0 },
            QueueLoss { queue: QueueId::NodeDown, capacity: 1_000, peak: 20, dropped: 0 },
        ]
    );
}

#[test]
fn analyze_rebases_c6_queue_drops_when_bridge_restarts() {
    // 5 before the capture are a baseline. The C6 restarts at a row that carries no
    // queue, so the next row's 15 are all new though they read as a rise from 12.
    let (_dir, conn) = capture(vec![
        queue_status(60_000, QueueId::RadioUp, 10, 5),
        queue_status(65_000, QueueId::RadioUp, 10, 12),
        status(0, 0, 0, 1_000),
        queue_status(4_000, QueueId::RadioUp, 10, 15),
    ]);
    let queues = losses(&conn).unwrap().bridge.unwrap().queues;
    assert_eq!(queues[0].dropped, 7 + 15);
}

#[test]
fn analyze_counts_each_queue_by_its_chip_when_c6_restarts() {
    // The C6's queues restart with it and count whole. The P4's run on, so they count only their
    // rise rather than their whole total again.
    let (_dir, conn) = capture(vec![
        queues_status(
            60_000,
            &[
                (QueueId::RadioUp, 2),
                (QueueId::NodeDown, 1),
                (QueueId::RadioDown, 4),
                (QueueId::HostPriority, 3),
                (QueueId::HostBulk, 500),
            ],
        ),
        queues_status(
            65_000,
            &[
                (QueueId::RadioUp, 6),
                (QueueId::NodeDown, 3),
                (QueueId::RadioDown, 4),
                (QueueId::HostPriority, 3),
                (QueueId::HostBulk, 512),
            ],
        ),
        queues_status(
            4_000,
            &[
                (QueueId::RadioUp, 3),
                (QueueId::NodeDown, 5),
                (QueueId::RadioDown, 6),
                (QueueId::HostPriority, 4),
                (QueueId::HostBulk, 515),
            ],
        ),
    ]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!(bridge.reboots, 1);
    let dropped = |queue| bridge.queues.iter().find(|q| q.queue == queue).unwrap().dropped;
    assert_eq!(dropped(QueueId::RadioUp), 4 + 3);
    assert_eq!(dropped(QueueId::NodeDown), 2 + 2);
    assert_eq!(dropped(QueueId::RadioDown), 2);
    assert_eq!(dropped(QueueId::HostPriority), 1);
    assert_eq!(dropped(QueueId::HostBulk), 12 + 3);
}

#[test]
fn analyze_totals_dropped_from_queues_when_c6_restarts() {
    // `dropped_tx` reads 508, 522, 505, and rebasing that fall whole would count the P4's 502 again.
    // The total is 12 + 2 before the restart, then 3 from the C6's new life and none from the P4.
    // `node-down` is not in `dropped_tx`.
    let (_dir, conn) = capture(vec![
        queues_status(
            60_000,
            &[(QueueId::RadioUp, 8), (QueueId::NodeDown, 1), (QueueId::HostBulk, 500)],
        ),
        queues_status(
            65_000,
            &[(QueueId::RadioUp, 20), (QueueId::NodeDown, 9), (QueueId::HostBulk, 502)],
        ),
        queues_status(
            4_000,
            &[(QueueId::RadioUp, 3), (QueueId::NodeDown, 9), (QueueId::HostBulk, 502)],
        ),
    ]);
    assert_eq!(losses(&conn).unwrap().bridge.unwrap().dropped, 12 + 2 + 3);
}

#[test]
fn analyze_rebases_p4_queue_drops_when_count_falls() {
    // A P4 reboot zeroes its queues, so the count falls and all of it is new.
    let (_dir, conn) = capture(vec![
        queues_status(60_000, &[(QueueId::HostBulk, 5)]),
        queues_status(65_000, &[(QueueId::HostBulk, 12)]),
        queues_status(4_000, &[(QueueId::HostBulk, 3)]),
    ]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!(bridge.queues[0].dropped, 7 + 3);
    assert_eq!(bridge.dropped, 7 + 3);
}

#[test]
fn analyze_sums_radio_link_errors_when_p4_reboots() {
    // The last row is a P4 reboot, which takes the C6 with it, so every counter falls and
    // rebases.
    let (_dir, conn) = capture(vec![
        radio_link_status(60_000, 1_000, 2),
        radio_link_status(65_000, 1_500, 5),
        radio_link_status(4_000, 100, 1),
    ]);
    let radio_link = losses(&conn).unwrap().bridge.unwrap().radio_link;
    // Each counter is its multiple of n, which advances 3 before the restart and 1 after.
    let n = 3 + 1;
    assert_eq!(
        radio_link,
        RadioLinkLoss {
            packets: 500 + 100,
            crc_fail: n,
            bad_magic: 2 * n,
            seq_gap: 3 * n,
            uart_errors: 4 * n,
            radio_crc_fail: 5 * n,
            radio_bad_magic: 6 * n,
            radio_seq_gap: 7 * n,
            radio_uart_errors: 8 * n,
        }
    );
}

/// A status reply carrying the P4's host UART errors.
fn host_uart_status(uptime_ms: u32, host_uart_errors: u32) -> Record {
    let Record::BridgeStatus(seen) = status(0, 0, 0, uptime_ms) else { unreachable!() };
    Record::BridgeStatus(BridgeStatusSeen { host_uart_errors, ..seen })
}

#[test]
fn analyze_sums_host_uart_errors_when_c6_restarts_and_p4_reboots() {
    // The second row is a C6 restart: the uptime falls, and the P4's count runs on. The third is
    // a P4 reboot, where the count itself falls and rebases.
    let (_dir, conn) = capture(vec![
        host_uart_status(60_000, 3),
        host_uart_status(1_000, 10),
        host_uart_status(500, 2),
        host_uart_status(5_500, 6),
    ]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!(bridge.host_uart_errors, 7 + 2 + 4);
}

#[test]
fn analyze_rebases_only_radio_counters_when_c6_restarts() {
    // The uptime falls, and the C6's counters with it. The P4's run on, so they count only their
    // rise rather than their whole total again.
    let (_dir, conn) = capture(vec![
        radio_link_status_split(60_000, 1_000, 2, 2),
        radio_link_status_split(65_000, 1_500, 5, 5),
        radio_link_status_split(4_000, 1_600, 7, 1),
    ]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!(bridge.reboots, 1);
    // The P4's multiple advances 3 then 2, the C6's 3 then 1 from its new life.
    let (p4, c6) = (3 + 2, 3 + 1);
    assert_eq!(
        bridge.radio_link,
        RadioLinkLoss {
            packets: 500 + 100,
            crc_fail: p4,
            bad_magic: 2 * p4,
            seq_gap: 3 * p4,
            uart_errors: 4 * p4,
            radio_crc_fail: 5 * c6,
            radio_bad_magic: 6 * c6,
            radio_seq_gap: 7 * c6,
            radio_uart_errors: 8 * c6,
        }
    );
}

#[test]
fn analyze_skips_queue_rows_when_name_unknown() {
    let (dir, conn) = capture(vec![
        queue_status(60_000, QueueId::HostBulk, 300, 5),
        queue_status(65_000, QueueId::HostBulk, 700, 9),
    ]);
    // A name no `QueueId` gives, as a capture from another build could hold.
    Connection::open(dir.path().join("wartui.db"))
        .unwrap()
        .execute(
            "INSERT INTO bridge_queue (status_id, queue, capacity, high_water, dropped)
             SELECT id, 'host-mystery', 64, 32, 1 FROM bridge_status",
            [],
        )
        .unwrap();
    let queues = losses(&conn).unwrap().bridge.unwrap().queues;
    assert_eq!(
        queues,
        [QueueLoss { queue: QueueId::HostBulk, capacity: 1_000, peak: 700, dropped: 4 }]
    );
}

#[test]
fn analyze_reports_no_queues_when_bridge_has_none() {
    let (_dir, conn) = capture(vec![status(0, 10, 0, 60_000), status(5, 20, 0, 65_000)]);
    let bridge = losses(&conn).unwrap().bridge.unwrap();
    assert_eq!(bridge.queues, vec![]);
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
        beat(OTHER, 0, 1, 0, 0),
    ]);
    let loss = losses(&conn).unwrap();
    // With no carrier no node has a slot, so sorted by address, and a node with batch gaps but no
    // heartbeat is still listed.
    let summary: Vec<_> =
        loss.nodes.iter().map(|n| (n.mac, n.batches_lost, n.heartbeats)).collect();
    assert_eq!(summary, vec![(OTHER, 1, 1), (NODE, 7, 0)]);
}

#[test]
fn analyze_counts_refusal_rise_when_sweep_counter_wraps() {
    // A small forward modulo delta is a wrap, not a reboot, so only the rise counts.
    let (_dir, conn) =
        capture(vec![beat(NODE, 0, u32::MAX - 1, 10, 0), beat(NODE, 15_000, 2, 12, 0)]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.wifi_refused), (2, 2));
}

#[test]
fn analyze_counts_every_heartbeat_row_when_rows_repeat_or_replay() {
    // A heartbeat carries no sequence, so every row counts, replayed or repeated.
    let (_dir, conn) = capture(vec![
        replayed(NODE, 0, 30),
        beat(NODE, 1_000, 240, 0, 0),
        beat(NODE, 1_010, 240, 0, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!(node.heartbeats, 3);
}

#[test]
fn analyze_classifies_restart_when_counter_delta_reaches_half_range() {
    // A restart counts the refusals whole; otherwise only their rise counts.
    for (counter, refused) in [(0x7fff_ffff, 2), (0x8000_0000, 12)] {
        let (_dir, conn) =
            capture(vec![beat(NODE, 0, 0, 10, 0), beat(NODE, 10_000, counter, 12, 0)]);
        let node = &losses(&conn).unwrap().nodes[0];
        assert_eq!(node.wifi_refused, refused, "counter {counter:#x}");
    }
}

#[test]
fn analyze_rebases_refusals_per_node_when_arrivals_are_interleaved() {
    // Each node's counter is read against its own previous row, never the other node's.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 0, 0),
        beat(OTHER, 1_000, 100, 50, 0),
        beat(NODE, 10_000, 3, 4, 0),
        beat(OTHER, 21_000, 104, 53, 0),
        beat(NODE, 15_000, 1, 2, 0),
    ]);
    let loss = losses(&conn).unwrap();
    let summary: Vec<_> =
        loss.nodes.iter().map(|n| (n.mac, n.heartbeats, n.wifi_refused)).collect();
    // `OTHER` sorts first; `NODE` restarted at its third row, so its 2 count whole.
    assert_eq!(summary, vec![(OTHER, 2, 3), (NODE, 3, 4 + 2)]);
}

#[test]
fn analyze_rebases_refusals_when_node_reboots() {
    let (_dir, conn) = capture(vec![
        // 100 refused before the capture began: a baseline.
        beat(NODE, 0, 10, 100, 4),
        beat(NODE, 5_000, 11, 130, 6),
        // Rebooted: the counts restarted, so all of each is new.
        beat(NODE, 10_000, 1, 20, 1),
        beat(NODE, 11_000, 2, 25, 1),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.wifi_refused, node.ble_refused), (30 + 20 + 5, 2 + 1));
}

#[test]
fn analyze_walks_arrival_order_when_clock_steps_back() {
    // The host clock steps back 30 s after the second heartbeat. Arrival order still has the
    // counter rising, so nothing restarted.
    let (_dir, conn) = capture(vec![
        beat(NODE, 0, 1, 10, 0),
        beat(NODE, 5_000, 2, 12, 0),
        beat(NODE, -25_000, 3, 15, 0),
        beat(NODE, -20_000, 4, 16, 0),
    ]);
    let node = &losses(&conn).unwrap().nodes[0];
    assert_eq!((node.heartbeats, node.wifi_refused), (4, 6));
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
    // 30 records the bridge received never reached the host; a bridge restart leaves the
    // host's own count rising.
    let (_dir, conn) = capture(vec![
        read_status(0, 10_000, 0, 60_000, 4_000),
        read_status(5, 10_400, 0, 65_000, 4_390),
        read_status(10, 300, 0, 4_000, 4_670),
    ]);
    let loss = losses(&conn).unwrap();
    assert_eq!(
        loss.bridge,
        Some(BridgeLoss {
            received: 700,
            dropped: 0,
            reboots: 1,
            host_read: 670,
            usb_lost: 30,
            oversize: 0,
            queues: vec![],
            radio_link: RadioLinkLoss::default(),
            host_uart_errors: 0,
        })
    );
}

#[test]
fn analyze_excludes_bridge_drops_from_usb_lost_when_bridge_dropped_frames() {
    // 1,000 received, 10 of them evicted from the bridge's host queue and never sent, and 985
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
    // The first reply overtook 24 records still queued in the bridge, so the host had read
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
            incompatible: 30,
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

/// A bridge on `board`, which is what a capture learns its carrier from.
fn bridge_on(board: NodeBoard) -> Record {
    Record::Bridge(BridgeSeen {
        mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
        fw_version: "0.1.0".to_owned(),
        node_board: board,
    })
}

/// A heartbeat from `slot`.
fn beat_from(node: Mac, slot: Slot, at_ms: i64) -> Record {
    let Record::Heartbeat(hb) = beat(node, at_ms, 1, 0, 0) else { unreachable!() };
    Record::Heartbeat(Heartbeat { slot, ..hb })
}

const A3: Slot = Slot::new(0, 2).unwrap();
const B1: Slot = Slot::new(1, 0).unwrap();
const THIRD: Mac = [0x02, 0x00, 0x5E, 0x10, 0x90, 0x01];

/// Each node's address and slot, in the order a summary lists them.
type Listed = Vec<(Mac, Option<Slot>)>;

/// What `losses` and then `peaks` list.
fn listed(conn: &Connection) -> (Listed, Listed) {
    let loss = losses(conn).expect("reading losses");
    let peak = peaks(conn, &loss.slots()).expect("reading peaks");
    (
        loss.nodes.iter().map(|n| (n.mac, n.slot)).collect(),
        peak.nodes.iter().map(|n| (n.mac, n.slot)).collect(),
    )
}

#[test]
fn analyze_lists_nodes_in_slot_order_when_on_a_carrier() {
    // By address OTHER sorts first, so slot order is what puts NODE ahead of it.
    let (_dir, conn) = capture(vec![
        bridge_on(NodeBoard::Carrier14),
        beat_from(OTHER, B1, 0),
        beat_from(NODE, A3, 0),
        sighting(OTHER, RecordKind::Wifi, 0),
        sighting(NODE, RecordKind::Wifi, 0),
    ]);
    let expected = vec![(NODE, Some(A3)), (OTHER, Some(B1))];
    assert_eq!(listed(&conn), (expected.clone(), expected));
    assert_eq!(losses(&conn).unwrap().board, NodeBoard::Carrier14);
}

#[test]
fn analyze_lists_slotless_nodes_after_slots_by_address_when_node_has_no_heartbeat() {
    let (_dir, conn) = capture(vec![
        bridge_on(NodeBoard::Carrier14),
        beat_from(THIRD, B1, 0),
        gap(NODE, 10, 1),
        gap(OTHER, 10, 1),
        sighting(NODE, RecordKind::Wifi, 0),
        sighting(OTHER, RecordKind::Wifi, 0),
        sighting(THIRD, RecordKind::Wifi, 0),
    ]);
    let expected = vec![(THIRD, Some(B1)), (OTHER, None), (NODE, None)];
    assert_eq!(listed(&conn), (expected.clone(), expected));
}

#[test]
fn analyze_gives_no_slots_when_capture_has_no_carrier() {
    let (_dir, conn) = capture(vec![
        bridge_on(NodeBoard::NoCarrier),
        beat_from(NODE, A3, 0),
        sighting(NODE, RecordKind::Wifi, 0),
    ]);
    let expected = vec![(NODE, None)];
    assert_eq!(listed(&conn), (expected.clone(), expected));
    assert_eq!(losses(&conn).unwrap().board, NodeBoard::NoCarrier);
}

#[test]
fn analyze_gives_no_carrier_when_no_bridge_announced_itself() {
    let (_dir, conn) = capture(vec![beat_from(NODE, A3, 0)]);
    assert_eq!(losses(&conn).unwrap().board, NodeBoard::NoCarrier);
    assert_eq!(losses(&conn).unwrap().nodes[0].slot, None);
}

#[test]
fn analyze_lists_moved_node_once_under_latest_slot_when_node_moved() {
    // `rx_at` runs backwards here: latest means the highest `id`.
    let (_dir, conn) = capture(vec![
        bridge_on(NodeBoard::Carrier14),
        beat_from(NODE, A3, 5_000),
        beat_from(NODE, B1, 0),
        sighting(NODE, RecordKind::Wifi, 0),
    ]);
    let expected = vec![(NODE, Some(B1))];
    assert_eq!(listed(&conn), (expected.clone(), expected));
}

#[test]
fn analyze_lists_both_nodes_by_address_when_they_end_on_one_slot() {
    let (_dir, conn) = capture(vec![
        bridge_on(NodeBoard::Carrier14),
        beat_from(NODE, A3, 0),
        beat_from(OTHER, B1, 1_000),
        beat_from(OTHER, A3, 2_000),
    ]);
    let (loss, _) = listed(&conn);
    assert_eq!(loss, vec![(OTHER, Some(A3)), (NODE, Some(A3))]);
}

#[test]
fn analyze_gives_no_slot_when_latest_heartbeat_slot_is_not_text() {
    let (dir, conn) = capture(vec![
        bridge_on(NodeBoard::Carrier14),
        beat_from(NODE, A3, 0),
        beat_from(NODE, B1, 1_000),
    ]);
    Connection::open(dir.path().join("wartui.db"))
        .expect("opening to write")
        .execute("UPDATE heartbeat SET slot = x'FF' WHERE id = (SELECT MAX(id) FROM heartbeat)", [])
        .expect("writing a slot of the wrong type");
    let loss = losses(&conn).expect("a bad slot does not fail the report");
    assert_eq!(loss.nodes[0].slot, None, "not the older heartbeat's slot");
}

/// A node's capture holding one heartbeat per reading, then whether it reports a supply drop.
fn supply_drop_over(readings: &[Option<u16>]) -> bool {
    let records = readings.iter().zip(1..).map(|(&vdd_mv, n)| reading(NODE, n, vdd_mv)).collect();
    let (_dir, conn) = capture(records);
    losses(&conn).expect("the report").nodes[0].supply_drop
}

/// A heartbeat carrying a rail reading, its counter and arrival following `n`.
fn reading(node: Mac, n: u32, vdd_mv: Option<u16>) -> Record {
    let Record::Heartbeat(hb) = beat(node, i64::from(n) * 5_000, n, 0, 0) else { unreachable!() };
    Record::Heartbeat(Heartbeat { vdd_mv, ..hb })
}

#[test]
fn analyze_reports_no_supply_drop_when_low_reading_stays_put() {
    assert!(!supply_drop_over(&[Some(2920), Some(2920), Some(2920)]));
}

#[test]
fn analyze_reports_no_supply_drop_when_fall_is_just_under_threshold() {
    assert!(!supply_drop_over(&[Some(3304), Some(3160)]));
}

#[test]
fn analyze_reports_supply_drop_when_reading_falls() {
    assert!(supply_drop_over(&[Some(3304), Some(3148)]));
}

#[test]
fn analyze_reports_supply_drop_when_reading_later_recovers() {
    assert!(supply_drop_over(&[Some(3304), Some(3148), Some(3304)]));
}

#[test]
fn analyze_reports_no_supply_drop_when_every_reading_is_null() {
    assert!(!supply_drop_over(&[None, None]));
}

#[test]
fn analyze_takes_first_reading_as_baseline_when_earlier_rows_are_null() {
    assert!(!supply_drop_over(&[None, Some(3148), Some(3148)]));
}

#[test]
fn analyze_keeps_supply_baselines_apart_when_arrivals_are_interleaved() {
    // Each node holds steady, and the two differ by more than a drop.
    let (_dir, conn) = capture(vec![
        reading(NODE, 1, Some(3304)),
        reading(OTHER, 2, Some(2980)),
        reading(NODE, 3, Some(3304)),
        reading(OTHER, 4, Some(2980)),
    ]);
    let loss = losses(&conn).expect("the report");
    assert_eq!(loss.nodes.len(), 2);
    assert!(loss.nodes.iter().all(|n| !n.supply_drop), "{:?}", loss.nodes);
}

#[test]
fn analyze_treats_wrong_type_as_no_reading_when_vdd_is_not_an_integer() {
    let (dir, conn) = capture(vec![
        reading(NODE, 1, Some(3304)),
        reading(NODE, 2, Some(3304)),
        reading(NODE, 3, Some(3148)),
    ]);
    Connection::open(dir.path().join("wartui.db"))
        .expect("opening to write")
        .execute(
            "UPDATE heartbeat SET vdd_mv = x'FF' WHERE id = (SELECT MIN(id) FROM heartbeat)",
            [],
        )
        .expect("writing a reading of the wrong type");
    let loss = losses(&conn).expect("a bad reading does not fail the report");
    assert!(loss.nodes[0].supply_drop, "the baseline is the next good reading");
}
