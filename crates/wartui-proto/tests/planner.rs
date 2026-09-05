//! The channel-pool planner.
//!
//! The properties here are what stop a node being told to scan a channel the
//! operator excluded, or being handed a range that spans the gap between two
//! runs — which `MSG_ADMIN` cannot express.

use std::collections::BTreeSet;

use wartui_proto::plan::{
    ChannelPool, MAX_NODES, NODE_STAGGER_WINDOW_MS, NUM_SCAN_CHANNELS, SCAN_CHANNELS, plan,
    stagger_offset_ms,
};

const POOLS: [ChannelPool; 2] = [ChannelPool::Us, ChannelPool::All];

/// Every index a plan hands out during one phase.
fn covered(p: &wartui_proto::plan::Plan, phase: u8) -> Vec<u8> {
    (0..p.node_count())
        .filter_map(|n| p.range_for(n, phase))
        .flat_map(|r| r.start..=r.end)
        .collect()
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
    assert_eq!(ChannelPool::All.channel_count(), 40);
}

#[test]
fn us_pool_is_two_runs_and_all_is_one() {
    assert_eq!(ChannelPool::Us.runs().len(), 2, "the gap at channels 12-14 splits the US pool");
    assert_eq!(ChannelPool::All.runs().len(), 1);
}

#[test]
fn no_assigned_range_ever_straddles_a_gap() {
    for pool in POOLS {
        for nodes in 1..=u8::try_from(MAX_NODES).expect("fits") {
            let p = plan(pool, nodes).expect("valid fleet size");
            for phase in 0..p.phase_count() {
                for node in 0..nodes {
                    let Some(range) = p.range_for(node, phase) else { continue };
                    let within_one_run = pool
                        .runs()
                        .iter()
                        .any(|run| run.contains(range.start) && run.contains(range.end));
                    assert!(
                        within_one_run,
                        "{pool:?}/{nodes} nodes/phase {phase}: node {node} got {range:?}, \
                         which MSG_ADMIN cannot express"
                    );
                }
            }
        }
    }
}

#[test]
fn a_full_rotation_covers_the_pool_and_nothing_else() {
    for pool in POOLS {
        let allowed: BTreeSet<u8> = pool.runs().iter().flat_map(|r| r.start..=r.end).collect();
        for nodes in 1..=u8::try_from(MAX_NODES).expect("fits") {
            let p = plan(pool, nodes).expect("valid fleet size");
            let seen: BTreeSet<u8> = (0..p.phase_count()).flat_map(|ph| covered(&p, ph)).collect();
            assert_eq!(
                seen,
                allowed,
                "{pool:?} with {nodes} nodes did not cover the pool exactly over \
                 {} phase(s)",
                p.phase_count()
            );
        }
    }
}

#[test]
fn within_a_phase_nodes_do_not_overlap() {
    for pool in POOLS {
        for nodes in 1..=u8::try_from(MAX_NODES).expect("fits") {
            let p = plan(pool, nodes).expect("valid fleet size");
            for phase in 0..p.phase_count() {
                let indices = covered(&p, phase);
                let unique: BTreeSet<u8> = indices.iter().copied().collect();
                assert_eq!(
                    indices.len(),
                    unique.len(),
                    "{pool:?}/{nodes} nodes/phase {phase}: two nodes were given the \
                     same channel"
                );
            }
        }
    }
}

#[test]
fn node_indices_are_unique_and_fleet_wide() {
    // node_index drives the transmit stagger slot, so it must not restart per
    // run — two nodes sharing an index would key up simultaneously.
    for pool in POOLS {
        for nodes in 1..=u8::try_from(MAX_NODES).expect("fits") {
            let p = plan(pool, nodes).expect("valid fleet size");
            let assigned: Vec<u8> = (0..nodes).filter(|&n| p.range_for(n, 0).is_some()).collect();
            assert_eq!(
                assigned,
                (0..nodes).collect::<Vec<_>>(),
                "{pool:?} with {nodes} nodes left a gap in the index numbering"
            );
            assert_eq!(p.node_count(), nodes);
        }
    }
}

#[test]
fn only_a_lone_node_on_a_multi_run_pool_has_to_rotate() {
    for nodes in 1..=u8::try_from(MAX_NODES).expect("fits") {
        assert!(!plan(ChannelPool::All, nodes).expect("valid").rotates());
    }
    let lone = plan(ChannelPool::Us, 1).expect("valid");
    assert!(lone.rotates(), "one node cannot hold both US runs at once");
    assert_eq!(lone.phase_count(), 2);
    assert_eq!(lone.range_for(0, 0), Some(ChannelPool::Us.runs()[0]));
    assert_eq!(lone.range_for(0, 1), Some(ChannelPool::Us.runs()[1]));
    // Phases wrap, so a dwell timer can just keep incrementing.
    assert_eq!(lone.range_for(0, 2), lone.range_for(0, 0));

    for nodes in 2..=u8::try_from(MAX_NODES).expect("fits") {
        assert!(!plan(ChannelPool::Us, nodes).expect("valid").rotates(), "{nodes} nodes");
    }
}

#[test]
fn all_pool_with_one_node_reproduces_the_stock_assignment() {
    // A single node on the full table should get exactly what unassigned stock
    // firmware defaults to: 0..=39.
    let p = plan(ChannelPool::All, 1).expect("valid");
    let range = p.range_for(0, 0).expect("assigned");
    assert_eq!((range.start, range.end), (0, NUM_SCAN_CHANNELS - 1));
}

#[test]
fn all_pool_splits_match_the_firmware_arithmetic() {
    // The vendor split is start=(n*40)/count, end=((n+1)*40)/count-1
    // (`src/WiFiOps.cpp:507-508`). With a single run the planner must agree.
    for nodes in 1..=u8::try_from(MAX_NODES).expect("fits") {
        let p = plan(ChannelPool::All, nodes).expect("valid");
        for n in 0..nodes {
            let range = p.range_for(n, 0).expect("assigned");
            let count = u16::from(nodes);
            let total = u16::from(NUM_SCAN_CHANNELS);
            let expected_start = (u16::from(n) * total) / count;
            let expected_end = ((u16::from(n) + 1) * total) / count - 1;
            assert_eq!(
                (u16::from(range.start), u16::from(range.end)),
                (expected_start, expected_end),
                "{nodes} nodes, node {n}"
            );
        }
    }
}

#[test]
fn us_pool_apportions_nodes_in_proportion_to_run_length() {
    // 11 channels in the 2.4 GHz run, 23 in the 5 GHz run. Six nodes should
    // land 2/4, not 3/3.
    let p = plan(ChannelPool::Us, 6).expect("valid");
    let low = (0..6)
        .filter(|&n| ChannelPool::Us.runs()[0].contains(p.range_for(n, 0).expect("assigned").start))
        .count();
    assert_eq!(low, 2, "6 nodes over 11+23 channels should put 2 on the 2.4 GHz run");
}

#[test]
fn admin_messages_carry_the_snapshot_node_count() {
    // Sending a live count instead of the planned one is the firmware bug at
    // `src/WiFiOps.cpp:651`; the count must match the partition it came from.
    let p = plan(ChannelPool::Us, 5).expect("valid");
    for n in 0..5 {
        let admin = p.admin_for(n, 0, 9).expect("assigned");
        assert_eq!(admin.node_count, 5);
        assert_eq!(admin.node_index, n);
        assert_eq!(admin.assignment_version, 9);
        let range = p.range_for(n, 0).expect("assigned");
        assert_eq!((admin.start_channel_idx, admin.end_channel_idx), (range.start, range.end));
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
