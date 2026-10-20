//! The frame ring behind the P4's USB host queue and the C6's air queues.

use wartui_proto::frame_ring::FrameRing;

fn read_all<const N: usize>(ring: &mut FrameRing<N>) -> Vec<u8> {
    let mut out = [0u8; 64];
    let (n, _) = ring.read(&mut out);
    out[..n].to_vec()
}

#[test]
fn frame_ring_round_trips_frames_in_order() {
    let mut ring = FrameRing::<64>::new();
    assert!(ring.push(b"abc"));
    assert!(ring.push(b"de"));
    let mut out = [0u8; 2];
    assert_eq!(ring.read(&mut out), (2, false));
    assert_eq!(&out, b"ab");
    assert_eq!(ring.read(&mut out), (1, true));
    assert_eq!(&out[..1], b"c");
    assert_eq!(ring.read(&mut out), (2, true));
    assert_eq!(&out, b"de");
    assert!(ring.is_empty());
}

#[test]
fn frame_ring_evicts_oldest_whole_frame_when_full() {
    let mut ring = FrameRing::<16>::new();
    assert!(ring.push(b"11111"));
    assert!(ring.push(b"22222"));
    assert_eq!(ring.dropped(), 0);
    assert!(ring.push(b"33333"));
    assert_eq!(ring.dropped(), 1);
    assert_eq!(read_all(&mut ring), b"22222");
    assert_eq!(read_all(&mut ring), b"33333");
    assert!(ring.is_empty());
}

#[test]
fn frame_ring_keeps_in_progress_frame_when_evicting() {
    let mut ring = FrameRing::<16>::new();
    assert!(ring.push(b"11111"));
    assert!(ring.push(b"22222"));
    let mut one = [0u8; 1];
    assert_eq!(ring.read(&mut one), (1, false));
    assert!(ring.mid_frame());
    assert!(ring.push(b"33333"));
    assert_eq!(ring.dropped(), 1);
    // Frame 1 finishes intact, then frame 3: frame 2 was the one evicted.
    assert_eq!(read_all(&mut ring), b"1111");
    assert_eq!(read_all(&mut ring), b"33333");
    assert!(ring.is_empty());
}

#[test]
fn frame_ring_refuses_frame_when_in_progress_frame_blocks_it() {
    let mut ring = FrameRing::<16>::new();
    assert!(ring.push(b"1111111111"));
    let mut one = [0u8; 1];
    assert_eq!(ring.read(&mut one), (1, false));
    assert!(!ring.push(b"22222"));
    assert_eq!(ring.dropped(), 1);
    assert_eq!(read_all(&mut ring), b"111111111");
}

#[test]
fn frame_ring_refuses_frame_when_larger_than_the_ring() {
    let mut ring = FrameRing::<16>::new();
    assert!(ring.push(b"1"));
    assert!(!ring.push(&[0u8; 15]));
    assert_eq!(ring.dropped(), 1);
    assert_eq!(read_all(&mut ring), b"1");
}

#[test]
fn frame_ring_wraps_frames_when_they_cross_the_end() {
    let mut ring = FrameRing::<16>::new();
    for round in 0..20u8 {
        assert!(ring.push(&[round; 6]));
        assert_eq!(read_all(&mut ring), [round; 6]);
    }
    assert_eq!(ring.dropped(), 0);
}

#[test]
fn frame_ring_reports_high_water_in_bytes() {
    let mut ring = FrameRing::<64>::new();
    assert!(ring.push(b"12345"));
    assert_eq!(read_all(&mut ring), b"12345");
    assert_eq!(ring.high_water(), 7);
    assert_eq!(ring.len_bytes(), 0);
}

#[test]
fn frame_ring_slides_wrapped_frame_when_evicting_a_shorter_one_behind_it() {
    let mut ring = FrameRing::<16>::new();
    // Move the head to 12, so the next frame crosses the end of the buffer.
    assert!(ring.push(&[0u8; 10]));
    assert_eq!(read_all(&mut ring), [0u8; 10]);
    assert!(ring.push(b"abcdefgh"));
    assert!(ring.push(b"x"));
    let mut one = [0u8; 1];
    assert_eq!(ring.read(&mut one), (1, false));
    // The 3-byte gap is shorter than the 10-byte frame read, so the slide overlaps itself
    // and wraps.
    assert!(ring.push(b"yy"));
    assert_eq!(ring.dropped(), 1);
    assert_eq!(read_all(&mut ring), b"bcdefgh");
    assert_eq!(read_all(&mut ring), b"yy");
    assert!(ring.is_empty());
}

#[test]
fn frame_ring_reads_only_later_frames_when_cleared() {
    let mut ring = FrameRing::<16>::new();
    assert!(ring.push(b"11111"));
    assert!(ring.push(b"22222"));
    assert!(ring.push(b"33333"));
    assert_eq!(ring.dropped(), 1);
    ring.clear();
    assert!(ring.is_empty());
    assert_eq!(ring.len_bytes(), 0);
    assert_eq!(ring.dropped(), 1);
    assert!(read_all(&mut ring).is_empty());
    assert!(ring.push(b"44"));
    assert_eq!(read_all(&mut ring), b"44");
}

#[test]
fn frame_ring_starts_next_frame_from_its_head_when_cleared_mid_frame() {
    let mut ring = FrameRing::<16>::new();
    assert!(ring.push(b"11111"));
    let mut one = [0u8; 1];
    assert_eq!(ring.read(&mut one), (1, false));
    ring.clear();
    assert!(!ring.mid_frame());
    assert!(ring.push(b"222"));
    assert_eq!(ring.read(&mut [0u8; 64][..]), (3, true));
    assert_eq!(ring.dropped(), 0);
}
