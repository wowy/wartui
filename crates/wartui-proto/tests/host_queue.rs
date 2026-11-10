//! The P4's host queue: priority before bulk at frame boundaries, and the chunk it writes.

use wartui_proto::host_queue::{HOST_CHUNK, HostQueue};

fn chunk<const P: usize, const B: usize>(queue: &mut HostQueue<P, B>) -> Vec<u8> {
    let mut out = [0u8; HOST_CHUNK];
    let n = queue.next_chunk(&mut out);
    out[..n].to_vec()
}

#[test]
fn host_queue_sends_priority_frame_before_queued_bulk() {
    let mut queue = HostQueue::<1024, 16384>::new();
    let bulk: Vec<u8> = (0..5000).map(|i| (i % 251) as u8 + 1).collect();
    let prio = [0xEEu8; 20];
    assert!(queue.push(&bulk, false));
    let first = chunk(&mut queue);
    assert_eq!(first, &bulk[..HOST_CHUNK]);
    assert!(queue.push(&prio, true));
    let mut second = bulk[HOST_CHUNK..].to_vec();
    second.extend_from_slice(&prio);
    assert_eq!(chunk(&mut queue), second);
    assert!(chunk(&mut queue).is_empty());
}

#[test]
fn host_queue_takes_priority_first_when_both_wait_at_a_boundary() {
    let mut queue = HostQueue::<1024, 4096>::new();
    assert!(queue.push(&[1u8; 10], false));
    assert!(queue.push(&[2u8; 10], true));
    let mut want = vec![2u8; 10];
    want.extend_from_slice(&[1u8; 10]);
    assert_eq!(chunk(&mut queue), want);
}

#[test]
fn host_queue_sends_nothing_more_when_frame_fills_chunk_exactly() {
    // A UART has no packet boundary for a host to wait on, so a full chunk owes nothing.
    let mut queue = HostQueue::<1024, 16384>::new();
    assert!(queue.push(&[7u8; HOST_CHUNK], false));
    assert_eq!(chunk(&mut queue).len(), HOST_CHUNK);
    assert!(chunk(&mut queue).is_empty());
}

#[test]
fn host_queue_fills_chunk_from_several_frames_when_they_are_queued() {
    let mut queue = HostQueue::<1024, 16384>::new();
    assert!(queue.push(&[7u8; 3000], false));
    assert!(queue.push(&[8u8; 3000], false));
    let mut want = vec![7u8; 3000];
    want.extend_from_slice(&[8u8; HOST_CHUNK - 3000]);
    assert_eq!(chunk(&mut queue), want);
    assert_eq!(chunk(&mut queue), [8u8; 6000 - HOST_CHUNK]);
    assert!(chunk(&mut queue).is_empty());
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
    use wartui_proto::{BridgeToHost, host_queue::is_priority};
    let log = BridgeToHost::Log { message: Default::default() };
    assert!(!is_priority(&log));
    let err = BridgeToHost::Error { message: Default::default() };
    assert!(is_priority(&err));
}

#[test]
fn host_queue_sends_nothing_when_cleared_with_frames_queued() {
    let mut queue = HostQueue::<1024, 4096>::new();
    assert!(queue.push(&[1u8; 10], false));
    assert!(queue.push(&[2u8; 10], true));
    queue.clear();
    assert!(chunk(&mut queue).is_empty());
    let [priority, bulk] = queue.stats();
    assert_eq!((priority.dropped, bulk.dropped), (0, 0));
}

#[test]
fn host_queue_starts_next_chunk_with_terminator_when_cleared_mid_frame() {
    let mut queue = HostQueue::<1024, 16384>::new();
    assert!(queue.push(&[7u8; 5000], false));
    assert_eq!(chunk(&mut queue).len(), HOST_CHUNK);
    queue.clear();
    assert!(queue.push(&[8u8; 10], true));
    let mut want = vec![0x00];
    want.extend_from_slice(&[8u8; 10]);
    assert_eq!(chunk(&mut queue), want);
    assert!(chunk(&mut queue).is_empty());
    let [priority, bulk] = queue.stats();
    assert_eq!((priority.dropped, bulk.dropped), (0, 0));
}

#[test]
fn host_queue_sends_lone_terminator_when_cleared_mid_frame_and_nothing_follows() {
    let mut queue = HostQueue::<1024, 16384>::new();
    assert!(queue.push(&[7u8; 5000], false));
    assert_eq!(chunk(&mut queue).len(), HOST_CHUNK);
    queue.clear();
    assert_eq!(chunk(&mut queue), [0x00]);
    assert!(chunk(&mut queue).is_empty());
}

#[test]
fn host_queue_fills_short_slice_when_bluetooth_takes_less_than_a_chunk() {
    // The Bluetooth sink takes the stream a packet's room at a time, not a chunk at a time.
    let mut queue = HostQueue::<1024, 16384>::new();
    assert!(queue.push(&[7u8; 300], false));
    let mut out = [0u8; 100];
    assert_eq!(queue.fill(&mut out), 100);
    let mut got = out.to_vec();
    // The priority frame waits for the bulk frame part-way out.
    assert!(queue.push(&[8u8; 30], true));
    loop {
        let n = queue.fill(&mut out);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&out[..n]);
    }
    let mut want = vec![7u8; 300];
    want.extend_from_slice(&[8u8; 30]);
    assert_eq!(got, want);
    assert_eq!(queue.fill(&mut []), 0);
}

#[test]
fn host_queue_reports_bulk_room_when_frames_wait() {
    let mut queue = HostQueue::<16, 64>::new();
    assert_eq!(queue.bulk_room(), 64);
    assert!(queue.push(&[1u8; 10], false));
    // Each frame sits behind its two-byte length.
    assert_eq!(queue.bulk_room(), 52);
    assert!(queue.push(&[2u8; 5], true));
    assert_eq!(queue.bulk_room(), 52);
}

#[test]
fn host_queue_classifies_spike_messages_when_deciding_priority() {
    use wartui_proto::link::SpikeBody;
    use wartui_proto::spike::SpikeStats;
    use wartui_proto::{BridgeToHost, host_queue::is_priority};
    assert!(!is_priority(&BridgeToHost::SpikeLoad { seq: 0, body: SpikeBody::new() }));
    assert!(is_priority(&BridgeToHost::SpikeStats(SpikeStats::default())));
}
