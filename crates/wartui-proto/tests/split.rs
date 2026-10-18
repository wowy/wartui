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
    pair.on_hello(hello(900), 1_000);
    pair
}

fn uptime(frame: Option<BridgeToHost>) -> u32 {
    match frame {
        Some(BridgeToHost::Ready { uptime_ms, .. }) => uptime_ms,
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
    assert_eq!(pair.on_hello(hello(900), 1_000), PairEvent::NewLife);
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
    let before = uptime(pair.ready(59_999));
    assert_eq!(pair.on_hello(hello(50), 60_000), PairEvent::NewLife);
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
    assert_eq!(pair.on_hello(other, 1_000), PairEvent::NewLife);
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
        queues: Vec::from_slice(&[stat(QueueId::RadioSendJobs, 9), stat(QueueId::RadioUp, 4)])
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
            assert_eq!(dropped_tx, 10);
            assert_eq!(uptime_ms, 2_900);
            let order: std::vec::Vec<QueueId> = queues.iter().map(|q| q.queue).collect();
            assert_eq!(
                order,
                [
                    QueueId::RadioSendJobs,
                    QueueId::RadioUp,
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
