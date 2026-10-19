//! The P4's pair state machine: radio up, `Ready`, routing and `Status`.

use heapless::Vec;
use wartui_proto::link::{
    BridgeToHost, Chip, HostToBridge, LoopPhase, QueueId, QueueStat, SendStatus, SpiStats,
};
use wartui_proto::reset::ResetCause;
use wartui_proto::spi::{Hello, LINK_REVISION, RadioStatus};
use wartui_proto::split::{Pair, PairEvent, RADIO_WAIT_MS, Route};

const OWN_MAC: [u8; 6] = [1, 1, 1, 1, 1, 1];
const C6_MAC: [u8; 6] = [6, 6, 6, 6, 6, 6];

fn pair() -> Pair {
    Pair::new(OWN_MAC, "p4-1".try_into().unwrap(), ResetCause::PowerOn, LoopPhase::Unknown, 0)
}

fn hello(uptime_ms: u32) -> Hello {
    Hello {
        link_rev: LINK_REVISION,
        chip: Chip::Esp32C6,
        mac: C6_MAC,
        fw_version: "c6-1".try_into().unwrap(),
        reset_cause: ResetCause::Software,
        last_phase: LoopPhase::Boot,
        uptime_ms,
        heap_free: 123_456,
    }
}

fn up_pair() -> Pair {
    let mut pair = pair();
    pair.on_hello(hello(900), 0, 1_000);
    pair
}

fn uptime(frame: Option<BridgeToHost>) -> u32 {
    match frame {
        Some(BridgeToHost::Ready { uptime_ms, .. }) => uptime_ms,
        other => panic!("not a Ready: {other:?}"),
    }
}

fn cause(frame: Option<BridgeToHost>) -> (ResetCause, LoopPhase) {
    match frame {
        Some(BridgeToHost::Ready { reset_cause, last_phase, .. }) => (reset_cause, last_phase),
        other => panic!("not a Ready: {other:?}"),
    }
}

fn send(id: u16) -> HostToBridge {
    HostToBridge::SendEspNow {
        id,
        dst: [9; 6],
        ensure_peer: false,
        payload: Vec::from_slice(&[1, 2, 3]).unwrap(),
    }
}

fn stat(queue: QueueId, dropped: u32) -> QueueStat {
    QueueStat { queue, capacity: 10, high_water: 5, dropped }
}

#[test]
fn pair_waits_then_announces_radio_mac_when_hello_arrives() {
    let mut pair = pair();
    assert_eq!(pair.ready(10), None);
    assert_eq!(pair.usb_serial_mac(), None);
    assert_eq!(pair.on_tick(RADIO_WAIT_MS - 1), PairEvent::None);
    assert_eq!(pair.on_hello(hello(900), 0, 1_000), PairEvent::NewLife);
    match pair.ready(1_000) {
        Some(BridgeToHost::Ready { chip, mac, radio, heap_free, .. }) => {
            assert_eq!(chip, Chip::Esp32P4);
            assert_eq!(mac, C6_MAC);
            assert_eq!(heap_free, 123_456);
            assert!(radio.is_some());
        }
        other => panic!("not a Ready: {other:?}"),
    }
    assert_eq!(pair.usb_serial_mac(), Some(C6_MAC));
    assert_eq!(pair.fault(), None);
}

#[test]
fn pair_announces_own_mac_and_error_when_radio_never_answers() {
    let mut pair = pair();
    assert_eq!(pair.on_tick(RADIO_WAIT_MS), PairEvent::NewLife);
    match pair.ready(RADIO_WAIT_MS) {
        Some(BridgeToHost::Ready { mac, radio, uptime_ms, .. }) => {
            assert_eq!(mac, OWN_MAC);
            assert_eq!(radio, None);
            assert_eq!(uptime_ms, 2_000);
        }
        other => panic!("not a Ready: {other:?}"),
    }
    assert!(pair.fault().unwrap().contains("not answering"));
    assert_eq!(pair.on_tick(RADIO_WAIT_MS + 1), PairEvent::None);
}

#[test]
fn pair_announces_new_life_when_hello_arrives_after_link_up() {
    let mut pair = up_pair();
    pair.note_seq(5_000);
    let before = uptime(pair.ready(59_999));
    assert_eq!(pair.on_hello(hello(50), 0, 60_000), PairEvent::NewLife);
    let after = uptime(pair.ready(60_000));
    assert_eq!(after, 50);
    assert!(after < before);
}

#[test]
fn pair_reports_c6_uptime_advancing_with_p4_clock() {
    let pair = up_pair();
    assert_eq!(uptime(pair.ready(3_000)), 2_900);
}

#[test]
fn pair_rejects_sends_and_reports_when_link_revision_differs() {
    let mut pair = pair();
    let mut other = hello(900);
    other.link_rev = LINK_REVISION + 1;
    assert_eq!(pair.on_hello(other, 0, 1_000), PairEvent::NewLife);
    assert_eq!(
        pair.route(&send(7)),
        Route::Refuse(Some(BridgeToHost::SendResult {
            id: 7,
            status: SendStatus::Rejected,
            tx_us: 0
        }))
    );
    assert_eq!(pair.route(&HostToBridge::SetTxPower { power: 40 }), Route::Refuse(None));
    let fault = pair.fault().unwrap();
    assert!(fault.contains(&(LINK_REVISION + 1).to_string()));
    assert!(fault.contains(&LINK_REVISION.to_string()));
    assert_eq!(pair.usb_serial_mac(), Some(C6_MAC));
}

#[test]
fn pair_routes_radio_commands_forward_when_up() {
    let pair = up_pair();
    assert_eq!(pair.route(&send(1)), Route::Forward);
    assert_eq!(pair.route(&HostToBridge::AddPeer { mac: [2; 6] }), Route::Forward);
    assert_eq!(pair.route(&HostToBridge::RemovePeer { mac: [2; 6] }), Route::Forward);
    assert_eq!(pair.route(&HostToBridge::SetTxPower { power: 40 }), Route::Forward);
}

#[test]
fn pair_routes_identify_local() {
    for pair in [pair(), up_pair()] {
        assert_eq!(pair.route(&HostToBridge::Identify), Route::Local);
        assert_eq!(pair.route(&HostToBridge::GetStatus), Route::Local);
        assert_eq!(pair.route(&HostToBridge::Reset), Route::Local);
    }
}

#[test]
fn pair_status_sums_host_and_radio_drops() {
    let mut pair = up_pair();
    pair.note_radio_status(RadioStatus {
        peer_count: 3,
        rx_count: 40,
        rx_oversize: 2,
        heap_free: 1,
        uptime_ms: 5_000,
        queues: Vec::from_slice(&[
            stat(QueueId::RadioSendJobs, 9),
            stat(QueueId::RadioUp, 4),
            stat(QueueId::RadioUpPriority, 2),
        ])
        .unwrap(),
        crc_fail: 11,
        bad_magic: 12,
        seq_gap: 13,
    });
    let spi = SpiStats {
        transfers: 100,
        crc_fail: 1,
        bad_magic: 2,
        seq_gap: 3,
        ready_timeouts: 4,
        ready_recovered: 5,
        radio_crc_fail: 0,
        radio_bad_magic: 0,
        radio_seq_gap: 0,
    };
    let host = [stat(QueueId::HostPriority, 0), stat(QueueId::HostBulk, 6)];
    let down = stat(QueueId::RadioDown, 1);
    match pair.status(3_000, host, down, spi, 6) {
        BridgeToHost::Status {
            peer_count,
            rx_count,
            dropped_tx,
            uptime_ms,
            rx_oversize,
            queues,
            spi: Some(spi),
        } => {
            assert_eq!((peer_count, rx_count, rx_oversize), (3, 40, 2));
            // The host's 6 and both of the C6's rings toward the P4, 4 and 2.
            assert_eq!(dropped_tx, 12);
            assert_eq!(uptime_ms, 2_900);
            let order: std::vec::Vec<QueueId> = queues.iter().map(|q| q.queue).collect();
            assert_eq!(
                order,
                [
                    QueueId::RadioSendJobs,
                    QueueId::RadioUp,
                    QueueId::RadioUpPriority,
                    QueueId::RadioDown,
                    QueueId::HostPriority,
                    QueueId::HostBulk
                ]
            );
            assert_eq!((spi.crc_fail, spi.transfers), (1, 100));
            assert_eq!((spi.radio_crc_fail, spi.radio_bad_magic, spi.radio_seq_gap), (11, 12, 13));
        }
        other => panic!("not a Status: {other:?}"),
    }
}

#[test]
fn pair_announces_new_life_when_first_hello_arrives() {
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
}

#[test]
fn pair_ignores_repeated_hellos_when_seq_rises() {
    // A real C6's first hello lands about 72 ms into its life, then repeats per transfer.
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    pair.note_radio_status(radio_status());
    assert_eq!(pair.on_hello(hello(73), 1, 1_001), PairEvent::None);
    assert_eq!(pair.on_hello(hello(74), 2, 1_002), PairEvent::None);
    assert_eq!(uptime(pair.ready(1_002)), 74);
    match pair.status(
        1_002,
        [stat(QueueId::HostPriority, 0), stat(QueueId::HostBulk, 0)],
        stat(QueueId::RadioDown, 0),
        spi_zero(),
        0,
    ) {
        BridgeToHost::Status { peer_count, .. } => assert_eq!(peer_count, 3),
        other => panic!("not a Status: {other:?}"),
    }
}

#[test]
fn pair_announces_new_life_when_hello_seq_restarts_with_same_uptime() {
    // The restarted C6 booted to the same first hello at the same uptime; only the seq,
    // back to 0 after transfers up to 40, tells the lives apart.
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    pair.note_seq(40);
    assert_eq!(pair.on_hello(hello(72), 0, 1_040), PairEvent::NewLife);
}

#[test]
fn pair_announces_new_life_when_c6_restarts_in_hello_phase() {
    // A panic 300 ms in, before the C6 has heard the P4's hello: hellos all the way.
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    for seq in 1..=3 {
        assert_eq!(pair.on_hello(hello(72 + seq), seq, 1_000 + u64::from(seq)), PairEvent::None);
    }
    assert_eq!(pair.on_hello(hello(72), 0, 1_300), PairEvent::NewLife);
}

#[test]
fn pair_announces_new_life_when_c6_restarts_long_into_life() {
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    for seq in 1..=100_000 {
        pair.note_seq(seq);
    }
    assert_eq!(pair.on_hello(hello(72), 0, 64_000), PairEvent::NewLife);
}

#[test]
fn pair_announces_radio_reset_cause_when_c6_restarts() {
    let mut pair = pair();
    pair.on_hello(hello(72), 0, 1_000);
    assert_eq!(cause(pair.ready(1_000)), (ResetCause::PowerOn, LoopPhase::Unknown));
    pair.note_seq(96_000);
    let mut crashed = hello(72);
    crashed.reset_cause = ResetCause::Watchdog;
    crashed.last_phase = LoopPhase::SpiTransfer;
    assert_eq!(pair.on_hello(crashed, 0, 60_000), PairEvent::NewLife);
    assert_eq!(cause(pair.ready(60_000)), (ResetCause::Watchdog, LoopPhase::SpiTransfer));
}

#[test]
fn pair_routes_panel_and_gps_local() {
    for pair in [pair(), up_pair()] {
        let panel = HostToBridge::ShowPanel { lines: Vec::new() };
        assert_eq!(pair.route(&panel), Route::Local);
        let gps = HostToBridge::GpsWrite { bytes: Vec::from_slice(&[0xb5, 0x62]).unwrap() };
        assert_eq!(pair.route(&gps), Route::Local);
    }
}

#[test]
fn pair_refuses_peer_commands_silently_when_radio_not_up() {
    let mut absent = pair();
    absent.on_tick(RADIO_WAIT_MS);
    for pair in [pair(), absent] {
        assert_eq!(pair.route(&HostToBridge::AddPeer { mac: [2; 6] }), Route::Refuse(None));
        assert_eq!(pair.route(&HostToBridge::RemovePeer { mac: [2; 6] }), Route::Refuse(None));
    }
}

#[test]
fn pair_announces_new_life_when_link_revision_changes() {
    let mut pair = up_pair();
    let mut other = hello(950);
    other.link_rev = LINK_REVISION + 1;
    assert_eq!(pair.on_hello(other, 1, 1_050), PairEvent::NewLife);
}

fn radio_status() -> RadioStatus {
    RadioStatus {
        peer_count: 3,
        rx_count: 0,
        rx_oversize: 0,
        heap_free: 0,
        uptime_ms: 0,
        queues: Vec::new(),
        crc_fail: 0,
        bad_magic: 0,
        seq_gap: 0,
    }
}

fn spi_zero() -> SpiStats {
    SpiStats {
        transfers: 0,
        crc_fail: 0,
        bad_magic: 0,
        seq_gap: 0,
        ready_timeouts: 0,
        ready_recovered: 0,
        radio_crc_fail: 0,
        radio_bad_magic: 0,
        radio_seq_gap: 0,
    }
}
