//! The channel-pool planner.
//!
//! The properties here are what stop a node being told to scan a channel the
//! operator excluded, or two nodes being given the same one while a third
//! covers nothing. They are properties rather than a comparison against another
//! implementation, and that is a deliberate loss: until Phase 2 the assignment
//! shape was the vendor core's, so `tests/golden.rs` could check the planner
//! against frames sniffed off a real one. It is wartui's own frame now, and
//! there is no second implementation left to disagree with.

use std::collections::BTreeSet;

use wartui_proto::plan::{
    ChannelPool, IndexRun, MAX_NODES, NODE_STAGGER_WINDOW_MS, NUM_SCAN_CHANNELS, SCAN_CHANNELS,
    UNSUPPORTED_INDEX, plan, stagger_offset_ms,
};

const POOLS: [ChannelPool; 2] = [ChannelPool::Us, ChannelPool::All];

const FLEET_SIZES: std::ops::RangeInclusive<u8> = 1..=20;

const _: () = assert!(*FLEET_SIZES.end() as usize == MAX_NODES);

/// Every index the plan hands to anybody, with repeats kept so overlap shows.
fn covered(p: &wartui_proto::plan::Plan) -> Vec<u8> {
    (0..p.node_count()).filter_map(|n| p.channels_for(n)).flat_map(|set| set.indices()).collect()
}

#[test]
fn scan_channel_table_matches_the_firmware() {
    assert_eq!(SCAN_CHANNELS.len(), usize::from(NUM_SCAN_CHANNELS));
    assert_eq!(&SCAN_CHANNELS[..14], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
    assert_eq!(&SCAN_CHANNELS[14..18], &[36, 40, 44, 48]);
    assert_eq!(SCAN_CHANNELS[39], 177);
}

#[test]
fn us_pool_excludes_exactly_the_channels_it_should() {
    let excluded: BTreeSet<u8> = [12, 13, 14, 169, 173, 177].into_iter().collect();
    for (idx, &channel) in SCAN_CHANNELS.iter().enumerate() {
        let idx = u8::try_from(idx).expect("table is 40 entries");
        assert_eq!(
            ChannelPool::Us.contains(idx),
            !excluded.contains(&channel),
            "channel {channel} (index {idx}) is on the wrong side of the US pool"
        );
    }
    assert_eq!(ChannelPool::Us.channel_count(), 34);
    assert_eq!(ChannelPool::All.channel_count(), 39, "39 and not 40: channel 14 is unsupported");
}

#[test]
fn no_pool_offers_the_one_channel_a_node_cannot_tune() {
    // `esp-radio` hardcodes `nchan: 13` in the country blob and exposes no way
    // to reach it, so a node handed index 13 refuses the hop once per sweep for
    // the life of the assignment and says so only on a serial console nobody is
    // watching. A pool containing it spends a dwell of every sweep on nothing.
    assert_eq!(SCAN_CHANNELS[usize::from(UNSUPPORTED_INDEX)], 14);
    for pool in POOLS {
        assert!(!pool.contains(UNSUPPORTED_INDEX), "{pool:?} offers channel 14");
        assert!(!pool.channels().contains(UNSUPPORTED_INDEX), "{pool:?} offers channel 14");
    }
    // Still in the table, because its indices are the wire format: removing an
    // entry would repoint every assignment in flight and every stored row.
    assert_eq!(SCAN_CHANNELS.len(), usize::from(NUM_SCAN_CHANNELS));
}

#[test]
fn both_pools_are_two_runs_because_both_have_a_hole_in_them() {
    assert_eq!(ChannelPool::Us.runs().len(), 2, "the gap at channels 12-14 splits the US pool");
    assert_eq!(ChannelPool::All.runs().len(), 2, "and channel 14 alone splits the All pool");
}

#[test]
fn one_plan_covers_the_pool_and_nothing_else() {
    // No phases: what the fleet holds at any moment is the whole pool. Before
    // the channel mask this could only be true after a full rotation, and a
    // lone node on the US pool was scanning half the pool at any instant.
    for pool in POOLS {
        let allowed: BTreeSet<u8> = pool.runs().iter().flat_map(|r| r.start..=r.end).collect();
        for nodes in FLEET_SIZES {
            let p = plan(pool, nodes).expect("valid fleet size");
            let seen: BTreeSet<u8> = covered(&p).into_iter().collect();
            assert_eq!(seen, allowed, "{pool:?} with {nodes} nodes did not cover the pool exactly");
        }
    }
}

#[test]
fn no_two_nodes_are_given_the_same_channel() {
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let p = plan(pool, nodes).expect("valid fleet size");
            let indices = covered(&p);
            let unique: BTreeSet<u8> = indices.iter().copied().collect();
            assert_eq!(
                indices.len(),
                unique.len(),
                "{pool:?}/{nodes} nodes: two nodes were given the same channel"
            );
        }
    }
}

#[test]
fn node_indices_are_unique_and_fleet_wide() {
    // node_index drives the transmit stagger slot, so it must not restart per
    // run — two nodes sharing an index would key up simultaneously.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let p = plan(pool, nodes).expect("valid fleet size");
            let assigned: Vec<u8> = (0..nodes).filter(|&n| p.channels_for(n).is_some()).collect();
            assert_eq!(
                assigned,
                (0..nodes).collect::<Vec<_>>(),
                "{pool:?} with {nodes} nodes left a gap in the index numbering"
            );
            assert_eq!(p.node_count(), nodes);
            assert_eq!(p.channels_for(nodes), None, "an index outside the fleet holds nothing");
        }
    }
}

#[test]
fn a_lone_node_holds_the_whole_pool_at_once() {
    // The property Phase 2 exists for. `MSG_ADMIN` used to carry one contiguous
    // range, so one node could not express the US pool's two runs together and
    // the plan rotated it between them on a sixty-second dwell — leaving half
    // the pool unscanned at every instant, and re-issuing an assignment (and
    // spending an epoch) every minute for as long as the fleet stayed at one.
    for pool in POOLS {
        let lone = plan(pool, 1).expect("one node is a valid fleet");
        assert_eq!(lone.channels_for(0), Some(pool.channels()));
        assert_eq!(
            lone.channels_for(0).expect("assigned").len(),
            u32::from(pool.channel_count()),
            "{pool:?}"
        );
    }
}

#[test]
fn all_pool_with_one_node_is_every_channel_that_node_can_tune() {
    // Not quite the whole table: stock firmware defaults to all forty indices
    // (`src/WiFiOps.cpp:77-80`), and one of them is a channel the radio refuses.
    let p = plan(ChannelPool::All, 1).expect("valid");
    let set = p.channels_for(0).expect("assigned");
    for idx in 0..NUM_SCAN_CHANNELS {
        assert_eq!(
            set.contains(idx),
            idx != UNSUPPORTED_INDEX,
            "index {idx} is on the wrong side of a lone node's whole-pool assignment"
        );
    }
}

#[test]
fn the_deal_is_round_robin_in_scan_channels_order() {
    // Index k of the pool's flattened order goes to node k % node_count. Said
    // out longhand here because everything below is a consequence of it, and a
    // planner that satisfied the consequences by some other means would be a
    // different planner with the same tests passing.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let p = plan(pool, nodes).expect("valid fleet size");
            let flattened: Vec<u8> = pool.runs().iter().flat_map(|r| r.start..=r.end).collect();
            for (k, idx) in flattened.into_iter().enumerate() {
                let owner = u8::try_from(k % usize::from(nodes)).expect("below node_count");
                assert!(
                    p.channels_for(owner).expect("assigned").contains(idx),
                    "{pool:?}/{nodes} nodes: index {idx} should have gone to node {owner}"
                );
            }
        }
    }
}

#[test]
fn shares_differ_by_at_most_one_channel() {
    // A node's sweep period is proportional to how many channels it holds, so
    // an uneven deal shows up as one node heartbeating visibly slower than the
    // rest — and as that node's observations being the stalest in the export.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let p = plan(pool, nodes).expect("valid fleet size");
            let sizes: Vec<u32> =
                (0..nodes).map(|n| p.channels_for(n).expect("assigned").len()).collect();
            let (low, high) = (
                *sizes.iter().min().expect("a fleet has nodes"),
                *sizes.iter().max().expect("a fleet has nodes"),
            );
            assert!(high - low <= 1, "{pool:?}/{nodes} nodes: shares ran {low}..={high}");
        }
    }
}

#[test]
fn every_node_gets_some_of_every_run_while_there_are_enough_channels() {
    // The reason for dealing rather than block-splitting. Eleven 2.4 GHz
    // channels and twenty-three 5 GHz ones would have gone to disjoint sets of
    // nodes under the old apportionment, so a node dropping out took a whole
    // band with it until the next re-cut landed.
    for pool in POOLS {
        let shortest = pool.runs().iter().map(IndexRun::len).min().expect("a pool has runs");
        for nodes in 1..=shortest.min(20) {
            let p = plan(pool, nodes).expect("valid fleet size");
            for n in 0..nodes {
                let set = p.channels_for(n).expect("assigned");
                for run in pool.runs() {
                    assert!(
                        (run.start..=run.end).any(|idx| set.contains(idx)),
                        "{pool:?}/{nodes} nodes: node {n} got nothing from {run:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn admin_messages_carry_the_snapshot_node_count() {
    // Sending a live count instead of the planned one is the firmware bug at
    // `src/WiFiOps.cpp:651`; the count must match the partition it came from.
    let p = plan(ChannelPool::Us, 5).expect("valid");
    for n in 0..5 {
        let admin = p.admin_for(n, 9, wartui_proto::air::ADMIN_FLAG_BLE).expect("assigned");
        assert_eq!(admin.node_count, 5);
        assert_eq!(admin.node_index, n);
        assert_eq!(admin.assignment_version, 9);
        assert_eq!(admin.channels, p.channels_for(n).expect("assigned"));
        // Flags are the caller's: which node scans Bluetooth is a decision
        // about one node, and the plan is a decision about the fleet.
        assert!(admin.scan_ble());
    }
}

#[test]
fn impossible_fleet_sizes_are_rejected() {
    assert!(plan(ChannelPool::Us, 0).is_none());
    assert!(plan(ChannelPool::Us, u8::try_from(MAX_NODES).expect("fits") + 1).is_none());
}

#[test]
fn stagger_matches_the_firmware_helper() {
    // Verbatim expectations from `calculateNodeStaggerOffsetMs`.
    assert_eq!(stagger_offset_ms(0, 1, NODE_STAGGER_WINDOW_MS), 0, "a lone node owns the channel");
    assert_eq!(stagger_offset_ms(3, 3, NODE_STAGGER_WINDOW_MS), 0, "index outside the fleet");
    assert_eq!(stagger_offset_ms(0, 4, NODE_STAGGER_WINDOW_MS), 0);
    assert_eq!(stagger_offset_ms(1, 4, NODE_STAGGER_WINDOW_MS), 30);
    assert_eq!(stagger_offset_ms(3, 4, NODE_STAGGER_WINDOW_MS), 90);
    // Truncating division keeps the last slot strictly inside the window.
    for count in 2..=u8::try_from(MAX_NODES).expect("fits") {
        for index in 0..count {
            assert!(
                stagger_offset_ms(index, count, NODE_STAGGER_WINDOW_MS) < NODE_STAGGER_WINDOW_MS
            );
        }
    }
}

#[test]
fn the_wire_epoch_cycles_through_every_value_the_firmware_will_accept() {
    use wartui_proto::air::wire_version;

    // Divergence 4. The host persists a `u64`; the wire field is one byte and
    // the firmware never puts 0 in it, so a node holding a freshly-zeroed field
    // must not be mistaken for one holding an assignment.
    assert_eq!(wire_version(1), 1);
    assert_eq!(wire_version(255), 255);
    assert_eq!(wire_version(256), 1, "255 distinct values, then round again");

    let seen: std::collections::BTreeSet<u8> = (1..=255).map(wire_version).collect();
    assert_eq!(seen.len(), 255);
    assert!(!seen.contains(&0), "zero is never sent");

    // And consecutive epochs always differ, which is the only property the
    // node actually checks: it adopts on `!=`, not on `>`.
    for counter in 1..1_000u64 {
        assert_ne!(wire_version(counter), wire_version(counter + 1));
    }
}
