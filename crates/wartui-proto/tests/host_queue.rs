//! The P4's USB host queue: priority before bulk at frame boundaries, and the packet rule.

use wartui_proto::host_queue::{HS_PACKET, HostQueue};

fn packet<const P: usize, const B: usize>(queue: &mut HostQueue<P, B>) -> Vec<u8> {
    let mut out = [0u8; HS_PACKET];
    let n = queue.next_packet(&mut out);
    out[..n].to_vec()
}

#[test]
fn host_queue_sends_priority_frame_before_queued_bulk() {
    let mut queue = HostQueue::<1024, 4096>::new();
    let bulk: Vec<u8> = (0..600).map(|i| (i % 251) as u8 + 1).collect();
    let prio = [0xEEu8; 20];
    assert!(queue.push(&bulk, false));
    let first = packet(&mut queue);
    assert_eq!(first, &bulk[..512]);
    assert!(queue.push(&prio, true));
    let mut second = bulk[512..].to_vec();
    second.extend_from_slice(&prio);
    assert_eq!(packet(&mut queue), second);
    assert!(packet(&mut queue).is_empty());
}

#[test]
fn host_queue_takes_priority_first_when_both_wait_at_a_boundary() {
    let mut queue = HostQueue::<1024, 4096>::new();
    assert!(queue.push(&[1u8; 10], false));
    assert!(queue.push(&[2u8; 10], true));
    let mut want = vec![2u8; 10];
    want.extend_from_slice(&[1u8; 10]);
    assert_eq!(packet(&mut queue), want);
}

#[test]
fn host_queue_appends_terminator_packet_when_frame_ends_on_packet_boundary() {
    let mut queue = HostQueue::<1024, 4096>::new();
    assert!(queue.push(&[7u8; 512], false));
    assert_eq!(packet(&mut queue).len(), 512);
    assert_eq!(packet(&mut queue), [0x00]);
    assert!(packet(&mut queue).is_empty());
}

#[test]
fn host_queue_sends_no_terminator_when_more_follows_full_packet() {
    let mut queue = HostQueue::<1024, 4096>::new();
    assert!(queue.push(&[7u8; 512], false));
    assert!(queue.push(&[8u8; 10], false));
    assert_eq!(packet(&mut queue).len(), 512);
    assert_eq!(packet(&mut queue), [8u8; 10]);
    assert!(packet(&mut queue).is_empty());
}

#[test]
fn host_queue_reports_bytes_and_drops_per_ring() {
    let mut queue = HostQueue::<16, 32>::new();
    assert!(queue.push(&[1u8; 5], true));
    assert!(queue.push(&[1u8; 5], false));
    assert!(!queue.push(&[1u8; 40], false));
    let [priority, bulk] = queue.stats();
    assert_eq!((priority.capacity, priority.high_water, priority.dropped), (16, 7, 0));
    assert_eq!((bulk.capacity, bulk.high_water, bulk.dropped), (32, 7, 1));
}

#[test]
fn host_queue_classifies_message_when_deciding_priority() {
    use wartui_proto::{BridgeToHost, host_queue::is_priority, link::LogLevel};
    let log = BridgeToHost::Log { level: LogLevel::Info, message: Default::default() };
    assert!(!is_priority(&log));
    let err = BridgeToHost::Error { message: Default::default() };
    assert!(is_priority(&err));
}
