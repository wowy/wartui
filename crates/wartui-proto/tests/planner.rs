//! The channel-pool planner.
//!
//! The properties here are what stop a node being told to scan a channel the
//! operator excluded, or two nodes being given the same one while a third covers
//! nothing. Properties rather than a comparison against another implementation,
//! because there is no second implementation left to disagree with.

use std::collections::BTreeSet;

use wartui_proto::plan::{
    ChannelPool, ChannelSet, FIRST_FIVE_GHZ_INDEX, IndexRun, MAX_NODES, NODE_STAGGER_WINDOW_MS,
    NUM_SCAN_CHANNELS, Radio, SCAN_CHANNELS, UNSUPPORTED_INDEX, is_five_ghz, plan, plan_for,
    stagger_offset_ms,
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
    // A node handed index 13 refuses the hop once per sweep, silently; see
    // `plan::UNSUPPORTED_INDEX`.
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
    // No phases: what the fleet holds at any moment is the whole pool.
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
    // A lone node holds both of the US pool's runs at once, which before the channel
    // mask took a rotation on a sixty-second dwell.
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
    // Not quite the whole table: one of the forty indices is a channel the
    // radio refuses.
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
    // Index k of the pool's flattened order goes to node k % node_count. Said out
    // longhand because everything below is a consequence of it, and a planner
    // satisfying those by other means would pass the same tests.
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
    // A sweep period is proportional to the share, so an uneven deal shows up as one
    // node's observations being the stalest in the export.
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
    // The reason for dealing rather than block-splitting: under a block split a node
    // dropping out takes a whole band with it until the next re-cut.
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
    // The planned count, not a live one: it must match the partition it came
    // from, or a node joining in between computes its stagger slot wrongly.
    let p = plan(ChannelPool::Us, 5).expect("valid");
    for n in 0..5 {
        let admin = p.admin_for(n, 9, wartui_proto::air::ADMIN_FLAG_BLE).expect("assigned");
        assert_eq!(admin.node_count, 5);
        assert_eq!(admin.node_index, n);
        assert_eq!(admin.epoch, 9);
        assert_eq!(admin.channels, p.channels_for(n).expect("assigned"));
        // Flags are the caller's: which node scans Bluetooth is a decision
        // about one node, and the plan is a decision about the fleet.
        assert!(admin.scan_ble());
    }
}

#[test]
fn a_uniform_fleet_gets_the_same_plan_by_either_route() {
    // `plan` is `plan_for` with every radio dual-band, so the mixed-fleet machinery
    // must be invisible to it. Every fleet on a bench today is one of these.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let radios = vec![Radio::DualBand; usize::from(nodes)];
            assert_eq!(plan(pool, nodes), plan_for(pool, &radios), "{pool:?}/{nodes}");
        }
    }
}

#[test]
fn a_two_point_four_radio_is_never_dealt_a_channel_it_cannot_tune() {
    // A C6 adopts a 5 GHz share, acknowledges it, and scans the part it can reach —
    // leaving a hole in the fleet's coverage with an assignment on top of it.
    for pool in POOLS {
        for dual in 0..4u8 {
            for narrow in 1..4u8 {
                let radios: Vec<Radio> = core::iter::repeat_n(Radio::DualBand, usize::from(dual))
                    .chain(core::iter::repeat_n(Radio::TwoPointFour, usize::from(narrow)))
                    .collect();
                let p = plan_for(pool, &radios).expect("a valid fleet");
                for (node, radio) in radios.iter().enumerate() {
                    let node = u8::try_from(node).expect("small fleet");
                    let Some(set) = p.channels_for(node) else { continue };
                    for idx in set.indices() {
                        assert!(
                            radio.can_tune(idx),
                            "{pool:?}: node {node} ({radio:?}) was given index {idx}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn channels_no_radio_present_can_tune_are_named_rather_than_dealt() {
    // A fleet of nothing but C6s covers eleven of the US pool's thirty-four. Not a
    // fault the planner can fix, and not one it should hide.
    let radios = [Radio::TwoPointFour; 3];
    let p = plan_for(ChannelPool::Us, &radios).expect("a valid fleet");
    let unreachable: BTreeSet<u8> = p.unreachable().indices().collect();
    assert!(unreachable.iter().all(|idx| is_five_ghz(*idx)), "only 5 GHz is out of reach");
    assert_eq!(unreachable.len(), 23, "every 5 GHz channel in the US pool");

    let dealt: BTreeSet<u8> = covered(&p).into_iter().collect();
    assert!(dealt.is_disjoint(&unreachable), "nothing unreachable was dealt anyway");
    assert_eq!(dealt.len() + unreachable.len(), usize::from(ChannelPool::Us.channel_count()));

    // And with one C5 among them there is no hole at all.
    let mixed = [Radio::DualBand, Radio::TwoPointFour, Radio::TwoPointFour];
    let p = plan_for(ChannelPool::Us, &mixed).expect("a valid fleet");
    assert_eq!(p.unreachable(), wartui_proto::plan::ChannelSet::empty());
}

#[test]
fn a_mixed_fleet_is_dealt_to_keep_the_slowest_node_as_fast_as_it_can_be() {
    // Dealt in pool order, the C5 would take its half of 2.4 GHz and then all
    // of 5 GHz on top: 29 channels against the C6's 5, which is the block split
    // this planner was written to avoid. Dealing the constrained channels first
    // costs nothing and gets the largest share down to 23.
    let p = plan_for(ChannelPool::Us, &[Radio::DualBand, Radio::TwoPointFour]).expect("valid");
    let c5 = p.channels_for(0).expect("assigned").len();
    let c6 = p.channels_for(1).expect("assigned").len();
    assert_eq!((c5, c6), (23, 11), "the C5 takes 5 GHz and the C6 takes 2.4");
    assert_eq!(c5 + c6, u32::from(ChannelPool::Us.channel_count()), "and between them, all of it");

    // Two C5s and a C6: the twenty-three 5 GHz channels go 12/11 to the C5s,
    // and the C6 is far enough behind to take the whole of 2.4 GHz.
    let three = [Radio::DualBand, Radio::DualBand, Radio::TwoPointFour];
    let p = plan_for(ChannelPool::Us, &three).expect("valid");
    let sizes: Vec<u32> = (0..3).map(|n| p.channels_for(n).expect("assigned").len()).collect();
    assert_eq!(sizes, vec![12, 11, 11]);
}

#[test]
fn impossible_fleet_sizes_are_rejected() {
    assert!(plan(ChannelPool::Us, 0).is_none());
    assert!(plan(ChannelPool::Us, u8::try_from(MAX_NODES).expect("fits") + 1).is_none());
    assert!(plan_for(ChannelPool::Us, &[]).is_none());
    assert!(plan_for(ChannelPool::Us, &[Radio::DualBand; MAX_NODES + 1]).is_none());
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
    use wartui_proto::air::wire_epoch;

    // The host persists a `u64`; the wire field is one byte and
    // the firmware never puts 0 in it, so a node holding a freshly-zeroed field
    // must not be mistaken for one holding an assignment.
    assert_eq!(wire_epoch(1), 1);
    assert_eq!(wire_epoch(255), 255);
    assert_eq!(wire_epoch(256), 1, "255 distinct values, then round again");

    let seen: std::collections::BTreeSet<u8> = (1..=255).map(wire_epoch).collect();
    assert_eq!(seen.len(), 255);
    assert!(!seen.contains(&0), "zero is never sent");

    // And consecutive epochs always differ, which is the only property the
    // node actually checks: it adopts on `!=`, not on `>`.
    for counter in 1..1_000u64 {
        assert_ne!(wire_epoch(counter), wire_epoch(counter + 1));
    }
}

#[test]
fn a_radio_keeps_only_the_part_of_a_set_it_can_tune() {
    // `plan_for` never deals an unreachable index, so this exists for the
    // paths that do not go through it — an assignment made by hand, which is
    // otherwise a way to hand one node exactly the share nobody scans.
    for pool in POOLS {
        let whole = pool.channels();
        assert_eq!(Radio::DualBand.tunable(whole), whole, "a C5 loses nothing");

        let narrowed = Radio::TwoPointFour.tunable(whole);
        assert!(!narrowed.is_empty(), "both pools start in 2.4 GHz");
        for idx in whole.indices() {
            assert_eq!(narrowed.contains(idx), !is_five_ghz(idx), "index {idx} of the {pool} pool");
        }
    }

    // Nothing but 5 GHz leaves nothing at all, which is the case the callers
    // have to treat as "no assignment" rather than as a narrower one.
    let mut five = ChannelSet::empty();
    five.insert(FIRST_FIVE_GHZ_INDEX);
    five.insert(NUM_SCAN_CHANNELS - 1);
    assert_eq!(Radio::DualBand.tunable(five), five);
    assert!(Radio::TwoPointFour.tunable(five).is_empty());
}
