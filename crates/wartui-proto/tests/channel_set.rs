//! The forty-two-bit channel mask an assignment carries.
//!
//! It is a small type, and every one of these properties is load-bearing
//! somewhere: the wire encoding is a contract with the node firmware, the
//! iteration order is the order a node sweeps in, and the out-of-range
//! behaviour is what stops a frame from a newer host stranding an older node.

use wartui_proto::plan::{
    CHANNEL_SET_BYTES, ChannelPool, ChannelSet, IndexRun, NUM_SCAN_CHANNELS, SCAN_CHANNELS,
    SweepCursor,
};

#[test]
fn an_empty_set_is_the_one_thing_a_node_is_never_sent() {
    let empty = ChannelSet::empty();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.first(), None);
    assert_eq!(empty.indices().count(), 0);
    assert_eq!(empty.to_bytes(), [0; CHANNEL_SET_BYTES]);
}

#[test]
fn a_run_becomes_the_indices_it_names() {
    let set = ChannelSet::from_run(IndexRun::new(14, 36));
    assert_eq!(set.indices().collect::<Vec<_>>(), (14..=36).collect::<Vec<_>>());
    assert_eq!(set.len(), 23);
    assert_eq!(set.first(), Some(14));
    assert!(set.contains(14) && set.contains(36));
    assert!(!set.contains(13) && !set.contains(37));

    // Both ends of the table, where an off-by-one in the shift would show.
    assert_eq!(ChannelSet::from_run(IndexRun::new(0, 0)).indices().collect::<Vec<_>>(), vec![0]);
    assert_eq!(ChannelSet::from_run(IndexRun::new(41, 41)).indices().collect::<Vec<_>>(), vec![41]);
    assert_eq!(ChannelSet::from_run(IndexRun::new(0, 41)).len(), u32::from(NUM_SCAN_CHANNELS));
}

#[test]
fn indices_come_back_in_scan_channels_order_which_is_the_order_a_node_sweeps() {
    // Not sorted by channel number: a node walks the set by index. Ascending index
    // happens to be ascending channel today, and this is what would catch a reorder.
    let mut set = ChannelSet::empty();
    for idx in [37, 3, 20, 0] {
        set.insert(idx);
    }
    let indices: Vec<u8> = set.indices().collect();
    assert_eq!(indices, vec![0, 3, 20, 37]);
    assert_eq!(set.indices().len(), 4, "the iterator knows its own length");

    let channels: Vec<u8> = indices.iter().map(|i| SCAN_CHANNELS[usize::from(*i)]).collect();
    assert_eq!(channels, vec![1, 4, 60, 161]);
}

#[test]
fn inserting_the_same_index_twice_is_not_two_channels() {
    let mut set = ChannelSet::empty();
    set.insert(9);
    set.insert(9);
    assert_eq!(set.len(), 1);
}

#[test]
fn an_index_this_build_cannot_scan_is_dropped_rather_than_rejected() {
    // A frame from a host that knows channels this firmware does not; see
    // `ChannelSet` for why the extra bits are dropped rather than the frame.
    let mut set = ChannelSet::empty();
    set.insert(0);
    set.insert(NUM_SCAN_CHANNELS);
    set.insert(63);
    assert_eq!(set.indices().collect::<Vec<_>>(), vec![0]);
    assert!(!set.contains(NUM_SCAN_CHANNELS));

    // The same on the way in from the wire, where the extra bits are in the
    // top of the six bytes rather than beyond them.
    assert_eq!(ChannelSet::from_bits(u64::MAX).len(), u32::from(NUM_SCAN_CHANNELS));
    assert_eq!(
        ChannelSet::from_bytes([0xFF; CHANNEL_SET_BYTES]).len(),
        u32::from(NUM_SCAN_CHANNELS)
    );
}

#[test]
fn the_six_wire_bytes_round_trip_and_are_little_endian() {
    for pool in [ChannelPool::Us, ChannelPool::Eu, ChannelPool::All] {
        let set = pool.channels();
        assert_eq!(ChannelSet::from_bytes(set.to_bytes()), set, "{pool:?}");
    }

    // Index 0 is the low bit of the first byte and index 41 the second bit of
    // the sixth: the two ends of the field, which is where a byte-order or
    // width mistake shows up first.
    let mut ends = ChannelSet::empty();
    ends.insert(0);
    ends.insert(41);
    assert_eq!(ends.to_bytes(), [0x01, 0x00, 0x00, 0x00, 0x00, 0x02]);
}

#[test]
fn a_pool_as_a_set_holds_exactly_what_the_pool_contains() {
    for pool in [ChannelPool::Us, ChannelPool::Eu, ChannelPool::All] {
        let set = pool.channels();
        assert_eq!(set.len(), u32::from(pool.channel_count()), "{pool:?}");
        for idx in 0..NUM_SCAN_CHANNELS {
            assert_eq!(set.contains(idx), pool.contains(idx), "{pool:?} index {idx}");
        }
    }
    // Two runs in one set, which is the whole reason for the mask.
    assert_eq!(ChannelPool::Us.channels().len(), 36);
    assert!(!ChannelPool::Us.channels().contains(11), "the gap at channels 12-14");
    assert_eq!(ChannelPool::Eu.channels().len(), 32);
    assert!(!ChannelPool::Eu.channels().contains(33), "channel 144 is outside the EU pool");
}

/// The seam between adopting an assignment and dwelling on it.
///
/// A node advances at the foot of every pass, after the dwell and the report.
/// A cursor that began life on the lowest assigned index would therefore be
/// stepped past before that channel was ever listened to.
#[test]
fn a_fresh_cursor_lands_on_the_first_channel_rather_than_stepping_over_it() {
    let mut set = ChannelSet::empty();
    for idx in [3, 9, 20] {
        set.insert(idx);
    }

    let mut cursor = SweepCursor::new();
    assert_eq!(cursor.index(), None, "nothing has been dwelt on yet");
    assert!(!cursor.advance(set), "the first step is not a completed sweep");
    assert_eq!(cursor.index(), Some(3), "the lowest assigned index, not the one after it");
}

#[test]
fn a_sweep_visits_every_assigned_channel_once_before_it_says_it_wrapped() {
    let mut set = ChannelSet::empty();
    for idx in [3, 9, 20] {
        set.insert(idx);
    }

    let mut cursor = SweepCursor::new();
    let mut visited = Vec::new();
    let mut wraps = 0;
    for _ in 0..7 {
        if cursor.advance(set) {
            wraps += 1;
        }
        visited.push(cursor.index().expect("a step always lands somewhere"));
    }
    assert_eq!(visited, vec![3, 9, 20, 3, 9, 20, 3]);
    assert_eq!(wraps, 2, "once at the end of each completed pass, and not before");
}

#[test]
fn a_single_channel_wraps_on_every_step_but_the_first() {
    // The narrowest assignment the view can make, and the one whose heartbeat
    // period the assignment test measures.
    let set = ChannelSet::from_run(IndexRun::new(5, 5));
    let mut cursor = SweepCursor::new();
    assert!(!cursor.advance(set));
    assert_eq!(cursor.index(), Some(5));
    assert!(cursor.advance(set), "one channel is a whole sweep");
    assert_eq!(cursor.index(), Some(5));
}

#[test]
fn re_assigning_a_node_starts_its_sweep_over_rather_than_where_it_left_off() {
    // Adoption replaces the set and resets the cursor together. Carrying the
    // old position across would skip every newly assigned index below it — and
    // a re-cut moves a node's whole share, so that is most of them.
    let old = ChannelSet::from_run(IndexRun::new(20, 25));
    let mut cursor = SweepCursor::new();
    while cursor.index() != Some(24) {
        cursor.advance(old);
    }

    let new = ChannelSet::from_run(IndexRun::new(0, 3));
    cursor = SweepCursor::new();
    assert!(!cursor.advance(new));
    assert_eq!(cursor.index(), Some(0), "the whole of the new set, from its lowest index");
}
