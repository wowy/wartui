//! The P4's pair state machine: radio up, `Ready` and `Status`.

use heapless::Vec;
use wartui_proto::bus::NodeBoard;
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::link::{
    BridgeToHost, LoopPhase, QueueId, QueueStat, RADIO_UPDATING, RadioLinkStats,
};
use wartui_proto::ota::TRIAL_BOOTS;
use wartui_proto::radio_link::{Chip, Hello, LINK_REVISION, RadioStatus};
use wartui_proto::reset::ResetCause;
use wartui_proto::split::{
    INSTALL_WAIT_MS, P4Counters, Pair, PairEvent, RADIO_SILENT_MS, RADIO_WAIT_MS, RadioState,
};

const OWN_MAC: [u8; 6] = [1, 1, 1, 1, 1, 1];
const C6_MAC: [u8; 6] = [6, 6, 6, 6, 6, 6];
/// The digest of the C6 image the P4 carries.
const BUNDLED: [u8; 32] = [0xB0; 32];
/// The digest of some other C6 build.
const OTHER: [u8; 32] = [0x0C; 32];

fn pair() -> Pair {
    Pair::new(
        OWN_MAC,
        "p4-1".try_into().unwrap(),
        ResetCause::PowerOn,
        LoopPhase::Unknown,
        0,
        BUNDLED,
    )
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
        image: BUNDLED,
    }
}

/// A hello from a C6 running `image`.
fn hello_running(uptime_ms: u32, image: [u8; 32]) -> Hello {
    Hello { image, ..hello(uptime_ms) }
}

/// A pair updating a C6 that runs [`OTHER`].
fn updating_pair() -> Pair {
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello_running(900, OTHER), 0, 1_000), PairEvent::NewLife);
    pair
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

fn stat(queue: QueueId, dropped: u32) -> QueueStat {
    QueueStat { queue, capacity: 10, high_water: 5, dropped }
}

#[test]
fn pair_waits_then_announces_radio_mac_when_hello_arrives() {
    let mut pair = pair();
    assert_eq!(pair.ready(10), None);
    assert_eq!(pair.bridge_mac(), None);
    assert_eq!(pair.on_tick(RADIO_WAIT_MS - 1), PairEvent::None);
    assert_eq!(pair.on_hello(hello(900), 0, 1_000), PairEvent::NewLife);
    match pair.ready(1_000) {
        Some(BridgeToHost::Ready { mac, radio, .. }) => {
            assert_eq!(mac, C6_MAC);
            assert!(radio.is_some());
        }
        other => panic!("not a Ready: {other:?}"),
    }
    assert_eq!(pair.bridge_mac(), Some(C6_MAC));
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
fn pair_refuses_records_and_reports_when_link_revision_differs() {
    let mut pair = pair();
    let mut other = hello(900);
    other.link_rev = LINK_REVISION + 1;
    assert_eq!(pair.on_hello(other, 0, 1_000), PairEvent::NewLife);
    assert!(!pair.bridges());
    let fault = pair.fault().unwrap();
    assert!(fault.contains(&(LINK_REVISION + 1).to_string()));
    assert!(fault.contains(&LINK_REVISION.to_string()));
    assert_eq!(pair.bridge_mac(), Some(C6_MAC));
}

#[test]
fn pair_status_sums_host_and_radio_drops() {
    let mut pair = up_pair();
    pair.note_radio_status(RadioStatus {
        queues: Vec::from_slice(&[stat(QueueId::RadioUp, 4)]).unwrap(),
        crc_fail: 11,
        bad_magic: 12,
        seq_gap: 13,
        uart_errors: 14,
    });
    let radio_link = RadioLinkStats {
        packets: 100,
        crc_fail: 1,
        bad_magic: 2,
        seq_gap: 3,
        uart_errors: 4,
        radio_crc_fail: 0,
        radio_bad_magic: 0,
        radio_seq_gap: 0,
        radio_uart_errors: 0,
    };
    let p4 = P4Counters {
        host: [stat(QueueId::HostPriority, 0), stat(QueueId::HostBulk, 6)],
        down: stat(QueueId::RadioDown, 1),
        node_down: stat(QueueId::NodeDown, 3),
        radio_link,
        host_dropped: 6,
        host_uart_errors: 9,
        rx_count: 40,
        rx_oversize: 2,
        fan: Some(FanStats { duty: FanDuty::new(40).expect("in range"), rpm: Some(1830) }),
    };
    match pair.status(3_000, p4) {
        BridgeToHost::Status {
            rx_count,
            dropped_tx,
            uptime_ms,
            rx_oversize,
            queues,
            radio_link,
            host_uart_errors,
            fan,
        } => {
            assert_eq!((rx_count, rx_oversize), (40, 2));
            assert_eq!(host_uart_errors, 9);
            assert_eq!(fan.and_then(|fan| fan.rpm), Some(1830), "passed through");
            // The host's 6 and the C6's ring toward the P4, 4; never the node queues' refusals.
            assert_eq!(dropped_tx, 10);
            assert_eq!(uptime_ms, 2_900);
            let order: std::vec::Vec<QueueId> = queues.iter().map(|q| q.queue).collect();
            assert_eq!(
                order,
                [
                    QueueId::RadioUp,
                    QueueId::RadioDown,
                    QueueId::NodeDown,
                    QueueId::HostPriority,
                    QueueId::HostBulk
                ]
            );
            assert_eq!((radio_link.crc_fail, radio_link.packets), (1, 100));
            assert_eq!(
                (radio_link.radio_crc_fail, radio_link.radio_bad_magic, radio_link.radio_seq_gap),
                (11, 12, 13)
            );
            assert_eq!((radio_link.uart_errors, radio_link.radio_uart_errors), (4, 14));
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
    // A real C6's first hello lands about 72 ms into its life, then repeats until the P4 answers.
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    pair.note_radio_status(radio_status());
    assert_eq!(pair.on_hello(hello(73), 1, 1_001), PairEvent::None);
    assert_eq!(pair.on_hello(hello(74), 2, 1_002), PairEvent::None);
    assert_eq!(uptime(pair.ready(1_002)), 74);
    match pair.status(1_002, p4_zero()) {
        BridgeToHost::Status { radio_link: link, .. } => assert_eq!(link.radio_crc_fail, 3),
        other => panic!("not a Status: {other:?}"),
    }
}

#[test]
fn pair_announces_new_life_when_hello_seq_restarts_with_same_uptime() {
    // The restarted C6 booted to the same first hello at the same uptime; only the seq,
    // back to 0 after packets up to 40, tells the lives apart.
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    pair.note_seq(40);
    assert_eq!(pair.on_hello(hello(72), 0, 1_040), PairEvent::NewLife);
}

#[test]
fn pair_announces_new_life_when_hello_seq_is_zero_after_last_seq_of_zero() {
    // A C6 that sent only its first hello before restarting: the seqs are equal, and a sender
    // never sends a packet twice, so this is a restart.
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    assert_eq!(pair.on_hello(hello(72), 0, 1_100), PairEvent::NewLife);
}

#[test]
fn pair_keeps_life_when_hello_seq_is_past_last_seq() {
    let mut pair = pair();
    assert_eq!(pair.on_hello(hello(72), 0, 1_000), PairEvent::NewLife);
    assert_eq!(pair.on_hello(hello(172), 1, 1_100), PairEvent::None);
    pair.note_seq(5);
    assert_eq!(pair.on_hello(hello(272), 6, 1_200), PairEvent::None);
    assert_eq!(pair.on_hello(hello(72), 6, 1_300), PairEvent::NewLife);
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
    crashed.last_phase = LoopPhase::RadioLink;
    assert_eq!(pair.on_hello(crashed, 0, 60_000), PairEvent::NewLife);
    assert_eq!(cause(pair.ready(60_000)), (ResetCause::Watchdog, LoopPhase::RadioLink));
}

#[test]
fn pair_ready_carries_node_board_when_given_one() {
    let mut pair = pair();
    pair.on_hello(hello(900), 0, 1_000);
    match pair.ready(1_000) {
        Some(BridgeToHost::Ready { node_board, .. }) => assert_eq!(node_board, NodeBoard::Bench),
        other => panic!("not a Ready: {other:?}"),
    }
    let mut pair = self::pair().with_node_board(NodeBoard::Carrier10);
    pair.on_hello(hello(900), 0, 1_000);
    match pair.ready(1_000) {
        Some(BridgeToHost::Ready { node_board, .. }) => {
            assert_eq!(node_board, NodeBoard::Carrier10);
        }
        other => panic!("not a Ready: {other:?}"),
    }
}

#[test]
fn pair_ready_carries_own_image_when_given_one() {
    let own = [0x4F; 32];
    let mut pair = pair().with_own_image(Some(own));
    assert_eq!(pair.on_hello(hello(900), 0, 1_000), PairEvent::NewLife);
    match pair.ready(1_000) {
        Some(BridgeToHost::Ready { image, .. }) => assert_eq!(image, Some(own)),
        other => panic!("not a Ready: {other:?}"),
    }
    let mut bare = self::pair();
    bare.on_hello(hello(900), 0, 1_000);
    assert!(matches!(bare.ready(1_000), Some(BridgeToHost::Ready { image: None, .. })));
}

#[test]
fn pair_announces_new_life_when_link_revision_changes() {
    let mut pair = up_pair();
    let mut other = hello(950);
    other.link_rev = LINK_REVISION + 1;
    assert_eq!(pair.on_hello(other, 1, 1_050), PairEvent::NewLife);
}

fn radio_status() -> RadioStatus {
    RadioStatus { queues: Vec::new(), crc_fail: 3, bad_magic: 0, seq_gap: 0, uart_errors: 0 }
}

fn p4_zero() -> P4Counters {
    P4Counters {
        host: [stat(QueueId::HostPriority, 0), stat(QueueId::HostBulk, 0)],
        down: stat(QueueId::RadioDown, 0),
        node_down: stat(QueueId::NodeDown, 0),
        radio_link: radio_link_zero(),
        host_dropped: 0,
        host_uart_errors: 0,
        rx_count: 0,
        rx_oversize: 0,
        fan: None,
    }
}

fn radio_link_zero() -> RadioLinkStats {
    RadioLinkStats {
        packets: 0,
        crc_fail: 0,
        bad_magic: 0,
        seq_gap: 0,
        uart_errors: 0,
        radio_crc_fail: 0,
        radio_bad_magic: 0,
        radio_seq_gap: 0,
        radio_uart_errors: 0,
    }
}

#[test]
fn pair_comes_up_when_hello_carries_bundled_digest() {
    let pair = up_pair();
    assert!(matches!(pair.state(), RadioState::Up(_)));
    assert!(pair.bridges());
    assert_eq!(pair.fault(), None);
}

#[test]
fn pair_updates_and_sends_no_records_when_digest_differs() {
    let mut pair = updating_pair();
    assert!(matches!(pair.state(), RadioState::Updating(_)));
    assert!(!pair.bridges());
    // The host still gets a Ready naming the C6, so it sees the fault.
    assert_eq!(pair.bridge_mac(), Some(C6_MAC));
    assert!(pair.ready(1_000).is_some());
    assert_eq!(pair.fault().unwrap(), "updating the radio to the bundled image (0%)");
    pair.note_update_progress(40);
    assert_eq!(pair.fault().unwrap(), "updating the radio to the bundled image (40%)");
}

#[test]
fn pair_fault_reads_full_progress_when_image_is_verified() {
    let mut pair = updating_pair();
    pair.note_update_progress(91);
    pair.note_update_progress(100);
    assert_eq!(pair.fault().unwrap(), "updating the radio to the bundled image (100%)");
}

#[test]
fn pair_fault_says_installing_when_install_is_sent() {
    let mut pair = updating_pair();
    pair.note_update_progress(100);
    pair.note_installed(9_000);
    assert_eq!(pair.fault().unwrap(), "updating the radio: installing the bundled image");
    // Still installing while the P4 resets a C6 that has not said hello.
    assert_eq!(pair.on_tick(9_000 + INSTALL_WAIT_MS), PairEvent::ResetRadio);
    assert_eq!(pair.fault().unwrap(), "updating the radio: installing the bundled image");
}

#[test]
fn pair_fault_starts_with_radio_updating_when_radio_is_updating() {
    // `wartui flash-bridge` follows the C6's update by this prefix.
    let mut updating = updating_pair();
    assert!(updating.fault().unwrap().starts_with(RADIO_UPDATING));
    updating.note_installed(9_000);
    assert!(updating.fault().unwrap().starts_with(RADIO_UPDATING));
    // A radio that is not updating says something else.
    let mut absent = pair();
    absent.on_tick(RADIO_WAIT_MS);
    assert!(!absent.fault().unwrap().starts_with(RADIO_UPDATING));
}

#[test]
fn pair_reads_records_when_updating_on_matching_revision() {
    assert!(updating_pair().reads_records());
    assert!(up_pair().reads_records());
}

#[test]
fn pair_reads_no_records_when_updating_on_other_revision() {
    let mut pair = pair();
    let mut other = hello_running(900, OTHER);
    other.link_rev = LINK_REVISION + 1;
    pair.on_hello(other, 0, 1_000);
    assert!(matches!(pair.state(), RadioState::Updating(_)));
    assert!(!pair.reads_records());
}

#[test]
fn pair_comes_up_with_new_life_when_hello_after_install_matches_bundle() {
    let mut pair = updating_pair();
    pair.note_seq(5_000);
    pair.note_installed(9_000);
    assert_eq!(pair.on_hello(hello(80), 0, 9_500), PairEvent::NewLife);
    assert!(matches!(pair.state(), RadioState::Up(_)));
    assert_eq!(pair.fault(), None);
    // The new life is the C6's, so the host sees its reset cause.
    assert_eq!(cause(pair.ready(9_500)), (ResetCause::Software, LoopPhase::Boot));
    // No reset follows a hello.
    assert_eq!(pair.on_tick(9_000 + 10 * INSTALL_WAIT_MS), PairEvent::None);
}

#[test]
fn pair_bridges_stale_with_fault_when_digest_still_differs_after_attempt() {
    let mut pair = updating_pair();
    pair.note_seq(5_000);
    pair.note_installed(9_000);
    // Rolled back to the build it ran before.
    assert_eq!(pair.on_hello(hello_running(80, OTHER), 0, 9_500), PairEvent::NewLife);
    assert!(matches!(pair.state(), RadioState::Stale(_)));
    assert!(pair.bridges());
    assert_eq!(pair.fault().unwrap(), "radio runs another build; the bundled image did not take");
}

#[test]
fn pair_refuses_stale_radio_when_link_revision_differs() {
    let mut pair = updating_pair();
    pair.note_seq(5_000);
    let mut other = hello_running(80, OTHER);
    other.link_rev = LINK_REVISION + 1;
    assert_eq!(pair.on_hello(other, 0, 9_500), PairEvent::NewLife);
    assert!(matches!(pair.state(), RadioState::Mismatch(_)));
    assert!(!pair.bridges());
}

#[test]
fn pair_goes_stale_when_update_fails() {
    let mut pair = updating_pair();
    pair.on_update_failed();
    assert!(matches!(pair.state(), RadioState::Stale(_)));
    assert!(pair.fault().unwrap().contains("did not take"));
}

#[test]
fn pair_updates_when_digest_is_zeros() {
    // A C6 that takes updates but could not read its own digest.
    let mut pair = pair();
    pair.on_hello(hello_running(900, [0; 32]), 0, 1_000);
    assert!(matches!(pair.state(), RadioState::Updating(_)));
}

#[test]
fn pair_resets_radio_trial_boots_times_then_gives_up_when_no_hello_follows_install() {
    let mut pair = updating_pair();
    pair.note_installed(10_000);
    assert_eq!(pair.on_tick(10_000 + INSTALL_WAIT_MS - 1), PairEvent::None);
    let mut now = 10_000;
    for _ in 0..TRIAL_BOOTS {
        now += INSTALL_WAIT_MS;
        assert_eq!(pair.on_tick(now), PairEvent::ResetRadio);
        assert_eq!(pair.on_tick(now + 1), PairEvent::None);
    }
    now += INSTALL_WAIT_MS;
    assert_eq!(pair.on_tick(now), PairEvent::NewLife);
    assert_eq!(pair.state(), &RadioState::Absent);
    assert!(pair.fault().unwrap().contains("not answering"));
    assert_eq!(pair.on_tick(now + INSTALL_WAIT_MS), PairEvent::None);
}

#[test]
fn pair_deadline_is_boot_wait_when_waiting() {
    assert_eq!(pair().next_deadline_ms(), Some(RADIO_WAIT_MS));
}

#[test]
fn pair_deadline_is_none_when_up() {
    assert_eq!(up_pair().next_deadline_ms(), None);
}

#[test]
fn pair_deadline_is_install_wait_when_installed() {
    let mut pair = updating_pair();
    pair.note_installed(10_000);
    assert_eq!(pair.next_deadline_ms(), Some(10_000 + INSTALL_WAIT_MS));
}

#[test]
fn pair_deadline_moves_forward_when_tick_resets_radio() {
    let mut pair = updating_pair();
    pair.note_installed(10_000);
    let now = 10_000 + INSTALL_WAIT_MS;
    assert_eq!(pair.on_tick(now), PairEvent::ResetRadio);
    assert_eq!(pair.next_deadline_ms(), Some(now + INSTALL_WAIT_MS));
}

#[test]
fn pair_deadline_is_none_when_hello_follows_install() {
    let mut pair = updating_pair();
    pair.note_seq(5_000);
    pair.note_installed(10_000);
    assert_eq!(pair.on_hello(hello(80), 0, 10_500), PairEvent::NewLife);
    assert_eq!(pair.next_deadline_ms(), None);
}

const SILENT_FAULT: &str = "the radio has stopped answering; wartui reset restarts the bridge";

#[test]
fn pair_faults_once_when_radio_stays_silent_after_poll() {
    let mut pair = up_pair();
    pair.note_asked(5_000);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS - 1), PairEvent::None);
    assert_eq!(pair.fault(), None);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS), PairEvent::RadioSilent);
    assert_eq!(pair.fault().unwrap(), SILENT_FAULT);
    // Said once, with no deadline left for the radio clock to spin on.
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS + 1), PairEvent::None);
    assert_eq!(pair.next_deadline_ms(), None);
}

#[test]
fn pair_faults_ahead_of_update_progress_when_updating_radio_stays_silent() {
    let mut pair = updating_pair();
    pair.note_asked(5_000);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS), PairEvent::RadioSilent);
    assert_eq!(pair.fault().unwrap(), SILENT_FAULT);
}

#[test]
fn pair_does_not_fault_when_no_poll_asked() {
    let mut pair = up_pair();
    assert_eq!(pair.on_tick(1_000_000), PairEvent::None);
    assert_eq!(pair.fault(), None);
}

#[test]
fn pair_keeps_first_poll_time_when_polls_repeat() {
    let mut pair = up_pair();
    pair.note_asked(5_000);
    pair.note_asked(6_000);
    assert_eq!(pair.next_deadline_ms(), Some(5_000 + RADIO_SILENT_MS));
}

#[test]
fn pair_clears_silent_fault_when_packet_heard() {
    let mut pair = up_pair();
    pair.note_asked(5_000);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS), PairEvent::RadioSilent);
    pair.note_heard();
    assert_eq!(pair.fault(), None);
    assert_eq!(pair.next_deadline_ms(), None);
    // The next poll starts a fresh wait.
    pair.note_asked(20_000);
    assert_eq!(pair.next_deadline_ms(), Some(20_000 + RADIO_SILENT_MS));
}

#[test]
fn pair_clears_silent_fault_when_radio_restarts() {
    let mut pair = up_pair();
    pair.note_asked(5_000);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS), PairEvent::RadioSilent);
    pair.note_seq(5_000);
    assert_eq!(pair.on_hello(hello(80), 0, 20_000), PairEvent::NewLife);
    assert_eq!(pair.fault(), None);
    assert_eq!(pair.next_deadline_ms(), None);
}

#[test]
fn pair_does_not_fault_when_silent_in_mismatch() {
    let mut pair = pair();
    // The bundled image, on another revision: nothing to update, and nothing to bridge.
    pair.on_hello(Hello { link_rev: LINK_REVISION + 1, ..hello(900) }, 0, 1_000);
    assert!(matches!(pair.state(), RadioState::Mismatch(_)));
    pair.note_asked(5_000);
    assert_eq!(pair.next_deadline_ms(), None);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS), PairEvent::None);
    assert!(pair.fault().unwrap().contains("link revision"));
}

#[test]
fn pair_drops_silent_deadline_when_failed_update_leaves_mismatch() {
    // Armed while updating, then a state that gets no records: a deadline left behind would
    // have the radio clock wake forever.
    let mut pair = pair();
    pair.on_hello(Hello { link_rev: LINK_REVISION + 1, ..hello_running(900, OTHER) }, 0, 1_000);
    pair.note_asked(5_000);
    pair.on_update_failed();
    assert!(matches!(pair.state(), RadioState::Mismatch(_)));
    assert_eq!(pair.next_deadline_ms(), None);
    assert_eq!(pair.on_tick(5_000 + RADIO_SILENT_MS), PairEvent::None);
}

#[test]
fn pair_does_not_fault_when_silent_while_installing() {
    let mut pair = updating_pair();
    pair.note_asked(5_000);
    pair.note_installed(6_000);
    pair.note_asked(7_000);
    assert_eq!(pair.next_deadline_ms(), Some(6_000 + INSTALL_WAIT_MS));
    assert_ne!(pair.on_tick(7_000 + RADIO_SILENT_MS), PairEvent::RadioSilent);
    assert!(pair.fault().unwrap().contains("installing"));
}

#[test]
fn pair_does_not_arm_silence_when_waiting_or_absent() {
    let mut pair = pair();
    pair.note_asked(100);
    assert_eq!(pair.next_deadline_ms(), Some(RADIO_WAIT_MS));
    assert_eq!(pair.on_tick(RADIO_WAIT_MS), PairEvent::NewLife);
    pair.note_asked(RADIO_WAIT_MS + 1);
    assert_eq!(pair.next_deadline_ms(), None);
}

#[test]
fn pair_deadline_is_silent_wait_when_poll_unanswered() {
    let mut pair = up_pair();
    pair.note_asked(5_000);
    assert_eq!(pair.next_deadline_ms(), Some(5_000 + RADIO_SILENT_MS));
}

#[test]
fn pair_goes_stale_without_reset_when_update_fails_after_install() {
    // The C6 refused the install, so it never reboots and no reset should follow.
    let mut pair = updating_pair();
    pair.note_installed(10_000);
    pair.on_update_failed();
    assert!(matches!(pair.state(), RadioState::Stale(_)));
    assert!(pair.fault().unwrap().contains("did not take"));
    assert_eq!(pair.next_deadline_ms(), None);
    assert_eq!(pair.on_tick(10_000 + INSTALL_WAIT_MS), PairEvent::None);
}

#[test]
fn pair_update_progress_reads_last_noted_percent() {
    let mut pair = updating_pair();
    assert_eq!(pair.update_progress(), 0);
    pair.note_update_progress(40);
    assert_eq!(pair.update_progress(), 40);
}
