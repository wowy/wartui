//! The channel-pool planner.
//!
//! The properties here are what stop a node being told to scan a channel the
//! operator excluded, or two nodes being given the same one while a third covers
//! nothing. Properties rather than a comparison against another implementation,
//! because there is no second implementation left to disagree with.

use std::collections::BTreeSet;

use wartui_proto::plan::{
    ChannelPool, ChannelSet, IndexRun, Job, MAX_NODES, NODE_STAGGER_WINDOW_MS, NUM_SCAN_CHANNELS,
    Radio, SCAN_CHANNELS, UNSUPPORTED_INDEX, is_five_ghz, plan, plan_for, stagger_offset_ms,
};

const POOLS: [ChannelPool; 3] = [ChannelPool::Us, ChannelPool::Eu, ChannelPool::All];

const FLEET_SIZES: std::ops::RangeInclusive<u8> = 1..=20;

const _: () = assert!(*FLEET_SIZES.end() as usize == MAX_NODES);

/// Every index the plan hands to anybody, with repeats kept so overlap shows.
fn covered(p: &wartui_proto::plan::Plan) -> Vec<u8> {
    (0..p.node_count()).filter_map(|n| p.channels_for(n)).flat_map(|set| set.indices()).collect()
}

#[test]
fn scan_channel_table_matches_firmware_definitions_when_checking_indices() {
    assert_eq!(SCAN_CHANNELS.len(), usize::from(NUM_SCAN_CHANNELS));
    assert_eq!(&SCAN_CHANNELS[..14], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
    assert_eq!(&SCAN_CHANNELS[14..18], &[36, 40, 44, 48]);
    assert_eq!(
        &SCAN_CHANNELS[22..34],
        &[100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144]
    );
    assert_eq!(SCAN_CHANNELS[41], 177);
}

#[test]
fn channel_set_excludes_unsupported_channels_when_constructing_us_pool() {
    let excluded: BTreeSet<u8> = [12, 13, 14, 169, 173, 177].into_iter().collect();
    for (idx, &channel) in SCAN_CHANNELS.iter().enumerate() {
        let idx = u8::try_from(idx).expect("table is 42 entries");
        assert_eq!(
            ChannelPool::Us.contains(idx),
            !excluded.contains(&channel),
            "channel {channel} (index {idx}) is on the wrong side of the US pool"
        );
    }
    assert_eq!(ChannelPool::Us.channel_count(), 36);
    assert_eq!(ChannelPool::All.channel_count(), 41, "41 and not 42: channel 14 is unsupported");
}

#[test]
fn channel_set_excludes_unsupported_channels_when_constructing_eu_pool() {
    // 5 GHz stops at 140: channel 144's twenty megahertz run past 5725, and
    // 149 upwards is another band. 2.4 GHz runs the whole way to 13.
    let excluded: BTreeSet<u8> =
        [14, 144, 149, 153, 157, 161, 165, 169, 173, 177].into_iter().collect();
    for (idx, &channel) in SCAN_CHANNELS.iter().enumerate() {
        let idx = u8::try_from(idx).expect("table is 42 entries");
        assert_eq!(
            ChannelPool::Eu.contains(idx),
            !excluded.contains(&channel),
            "channel {channel} (index {idx}) is on the wrong side of the EU pool"
        );
    }
    assert_eq!(ChannelPool::Eu.channel_count(), 32, "thirteen 2.4 GHz channels and nineteen 5 GHz");
}

#[test]
fn channel_pool_defaults_to_all_when_invoking_default() {
    // A pool bounds where a node listens, so the widest one is the one that
    // costs an operator nothing to be handed without asking.
    assert_eq!(ChannelPool::default(), ChannelPool::All);
}

#[test]
fn channel_pool_excludes_unsupported_channel_fourteen_when_enumerating_pools() {
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
fn channel_pool_partitions_into_two_runs_when_checking_pool_gaps() {
    assert_eq!(ChannelPool::Us.runs().len(), 2, "the gap at channels 12-14 splits the US pool");
    assert_eq!(ChannelPool::Eu.runs().len(), 2, "channel 14 alone splits the EU pool");
    assert_eq!(ChannelPool::All.runs().len(), 2, "and channel 14 alone splits the All pool");
}

#[test]
fn planner_covers_entire_pool_without_extraneous_channels_when_generating_plan() {
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
fn planner_partitions_channels_without_overlap_when_dividing_pool() {
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
fn planner_assigns_contiguous_unique_indices_when_planning_fleet() {
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
fn planner_assigns_full_pool_when_fleet_consists_of_single_node() {
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
fn planner_assigns_all_tunable_channels_when_planning_single_node_on_all_pool() {
    // Not quite the whole table: one of the forty-two indices is a channel the
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
fn planner_distributes_channels_round_robin_when_partitioning_pool() {
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
fn planner_balances_channel_counts_within_one_when_dealing_to_fleet() {
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
fn planner_distributes_all_index_runs_to_nodes_when_channel_count_permits() {
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
fn plan_encodes_fleet_metadata_into_admin_frame_when_generating_admin_msg() {
    // The planned count, not a live one: it must match the partition it came
    // from, or a node joining in between computes its stagger slot wrongly.
    let p = plan(ChannelPool::Us, 5).expect("valid");
    for n in 0..5 {
        let admin = p.admin_for(n, 9, wartui_proto::air::ADMIN_FLAG_BLE, 8).expect("assigned");
        assert_eq!(admin.node_count, 5);
        assert_eq!(admin.node_index, n);
        assert_eq!(admin.epoch, 9);
        assert_eq!(admin.channels, p.channels_for(n).expect("assigned"));
        // Flags are the caller's: which node scans Bluetooth is the operator's
        // decision, and the plan is what acts on it.
        assert!(admin.scan_ble());
        assert_eq!(admin.tx_power, 8);
    }
}

#[test]
fn planner_assigns_empty_channel_set_when_node_holds_ble_flag() {
    // Counted, because `node_index` and `node_count` are the fleet's stagger
    // arithmetic rather than a census of who is sniffing: cut it out of the
    // numbering and every other node's transmit slot moves.
    let fleet = [Job::Bluetooth, Job::Wifi(Radio::DualBand), Job::Wifi(Radio::DualBand)];
    let p = plan_for(ChannelPool::Us, &fleet).expect("a valid fleet");

    assert_eq!(p.node_count(), 3);
    assert_eq!(p.bluetooth(), Some(0));
    assert_eq!(p.channels_for(0), Some(ChannelSet::empty()), "dealt nothing, and told so");
    assert!(!p.channels_for(1).expect("assigned").is_empty());
    assert!(!p.channels_for(2).expect("assigned").is_empty());

    // The whole pool still goes out, across the two nodes left sniffing.
    let dealt: BTreeSet<u8> = covered(&p).into_iter().collect();
    assert_eq!(dealt.len(), usize::from(ChannelPool::Us.channel_count()));
    assert_eq!(p.unreachable(), ChannelSet::empty());
}

#[test]
fn planner_partitions_pool_across_wifi_nodes_only_when_ble_node_present() {
    // Three nodes, one of them on Bluetooth, deals the same shares as two nodes —
    // the point of the change, and the reason moving the scan has to re-cut.
    let two = plan(ChannelPool::Us, 2).expect("valid");
    let explicit = plan_for(ChannelPool::Us, &[Job::Wifi(Radio::DualBand); 2]).expect("valid");
    assert_eq!(two, explicit, "the convenience route and the explicit one agree");

    let with_scanner = plan_for(
        ChannelPool::Us,
        &[Job::Wifi(Radio::DualBand); 2]
            .iter()
            .copied()
            .chain([Job::Bluetooth])
            .collect::<Vec<_>>(),
    )
    .expect("valid");
    assert_eq!(with_scanner.channels_for(0), two.channels_for(0));
    assert_eq!(with_scanner.channels_for(1), two.channels_for(1));
    assert_eq!(with_scanner.node_count(), 3, "and the scanner is still a slot");
}

#[test]
fn planner_marks_entire_pool_unreachable_when_only_node_is_assigned_ble() {
    // A plan rather than a refusal: refusing would leave that node holding the
    // share it already had, still sniffing Wi-Fi, which is the opposite of what
    // was asked. The hole is real and `unreachable` is where it is reported.
    let p = plan_for(ChannelPool::Us, &[Job::Bluetooth]).expect("one node is a valid fleet");
    assert_eq!(p.channels_for(0), Some(ChannelSet::empty()));
    assert_eq!(p.unreachable(), ChannelPool::Us.channels());
    // And a caller can tell this apart from a missing 5 GHz radio without asking,
    // because every radio tunes 2.4 GHz: a 2.4 GHz channel is out of reach only
    // when nothing is sniffing at all.
    assert!(p.unreachable().indices().any(|idx| !is_five_ghz(idx)));
}

#[test]
fn planner_marks_five_ghz_unreachable_when_only_dual_band_node_assigned_ble() {
    // Not a mixed fleet: it is a one-radio fleet whose only dual-band node is
    // doing something else, so the 5 GHz pass must not run at all.
    let fleet = [Job::Bluetooth, Job::Wifi(Radio::TwoPointFour)];
    let p = plan_for(ChannelPool::Us, &fleet).expect("a valid fleet");
    assert!(p.unreachable().indices().all(is_five_ghz), "only 5 GHz is out of reach");
    assert_eq!(p.unreachable().len(), 25, "every 5 GHz channel in the US pool");
    assert_eq!(p.channels_for(1).expect("assigned").len(), 11, "the C6 takes all of 2.4");
}

#[test]
fn plan_refuses_empty_channel_set_when_node_lacks_ble_flag() {
    // The one frame that must never exist: an empty mask without the flag tells a
    // node to scan nothing, and a node sent one parks while the host goes on
    // believing it is sweeping.
    let p = plan_for(ChannelPool::Us, &[Job::Bluetooth, Job::Wifi(Radio::DualBand)])
        .expect("a valid fleet");
    assert!(
        p.admin_for(0, 1, wartui_proto::air::ADMIN_FLAG_BLE, 8)
            .is_some_and(|admin| admin.channels.is_empty())
    );
    assert!(p.admin_for(0, 1, 0, 8).is_none(), "an empty mask without the flag is refused");
    assert!(p.admin_for(1, 1, 0, 8).is_some(), "and a real share needs no flag");

    // A surplus slot is `None` either way: there is nothing to say to it, and the
    // flag does not invent something.
    let crowd = [Job::Wifi(Radio::TwoPointFour); 14];
    let p = plan_for(ChannelPool::Eu, &crowd).expect("a valid fleet");
    assert!(p.channels_for(13).is_none(), "thirteen 2.4 GHz channels across fourteen nodes");
    assert!(p.admin_for(13, 1, wartui_proto::air::ADMIN_FLAG_BLE, 8).is_none());

    // Every pool has 2.4 GHz in it and every radio tunes 2.4 GHz, so the fallback
    // the engine gives such a node is never itself empty.
    for pool in POOLS {
        for radio in [Radio::DualBand, Radio::TwoPointFour] {
            assert!(!pool.reachable_by(radio).is_empty(), "{pool:?}/{radio:?}");
            assert!(pool.reachable_by(radio).indices().all(|idx| radio.can_tune(idx)));
        }
    }
}

#[test]
fn planner_produces_identical_plan_when_using_convenience_or_explicit_api() {
    // `plan` is `plan_for` with every radio dual-band, so the mixed-fleet machinery
    // must be invisible to it. Every fleet on a bench today is one of these.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let fleet = vec![Job::Wifi(Radio::DualBand); usize::from(nodes)];
            assert_eq!(plan(pool, nodes), plan_for(pool, &fleet), "{pool:?}/{nodes}");
        }
    }
}

#[test]
fn planner_restricts_single_band_node_to_two_point_four_channels_when_dealing() {
    // A C6 adopts a 5 GHz share, acknowledges it, and scans the part it can reach —
    // leaving a hole in the fleet's coverage with an assignment on top of it.
    for pool in POOLS {
        for dual in 0..4u8 {
            for narrow in 1..4u8 {
                let radios: Vec<Radio> = core::iter::repeat_n(Radio::DualBand, usize::from(dual))
                    .chain(core::iter::repeat_n(Radio::TwoPointFour, usize::from(narrow)))
                    .collect();
                let fleet: Vec<Job> = radios.iter().copied().map(Job::from).collect();
                let p = plan_for(pool, &fleet).expect("a valid fleet");
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
fn planner_reports_untunable_channels_as_unreachable_when_five_ghz_radios_absent() {
    // A fleet of nothing but C6s covers eleven of the US pool's thirty-six. Not a
    // fault the planner can fix, and not one it should hide.
    let fleet = [Job::Wifi(Radio::TwoPointFour); 3];
    let p = plan_for(ChannelPool::Us, &fleet).expect("a valid fleet");
    let unreachable: BTreeSet<u8> = p.unreachable().indices().collect();
    assert!(unreachable.iter().all(|idx| is_five_ghz(*idx)), "only 5 GHz is out of reach");
    assert_eq!(unreachable.len(), 25, "every 5 GHz channel in the US pool");

    let dealt: BTreeSet<u8> = covered(&p).into_iter().collect();
    assert!(dealt.is_disjoint(&unreachable), "nothing unreachable was dealt anyway");
    assert_eq!(dealt.len() + unreachable.len(), usize::from(ChannelPool::Us.channel_count()));

    // And with one C5 among them there is no hole at all.
    let mixed = [
        Job::Wifi(Radio::DualBand),
        Job::Wifi(Radio::TwoPointFour),
        Job::Wifi(Radio::TwoPointFour),
    ];
    let p = plan_for(ChannelPool::Us, &mixed).expect("a valid fleet");
    assert_eq!(p.unreachable(), wartui_proto::plan::ChannelSet::empty());
}

#[test]
fn planner_optimizes_channel_shares_when_fleet_contains_mixed_radios() {
    // Dealt in pool order, the C5 would take its half of 2.4 GHz and then all
    // of 5 GHz on top: 31 channels against the C6's 5, which is the block split
    // this planner was written to avoid. Dealing the constrained channels first
    // costs nothing and gets the largest share down to 25.
    let p =
        plan_for(ChannelPool::Us, &[Job::Wifi(Radio::DualBand), Job::Wifi(Radio::TwoPointFour)])
            .expect("valid");
    let c5 = p.channels_for(0).expect("assigned").len();
    let c6 = p.channels_for(1).expect("assigned").len();
    assert_eq!((c5, c6), (25, 11), "the C5 takes 5 GHz and the C6 takes 2.4");
    assert_eq!(c5 + c6, u32::from(ChannelPool::Us.channel_count()), "and between them, all of it");

    // Two C5s and a C6: the twenty-five 5 GHz channels go 13/12 to the C5s,
    // and the C6 is far enough behind to take the whole of 2.4 GHz.
    let three =
        [Job::Wifi(Radio::DualBand), Job::Wifi(Radio::DualBand), Job::Wifi(Radio::TwoPointFour)];
    let p = plan_for(ChannelPool::Us, &three).expect("valid");
    let sizes: Vec<u32> = (0..3).map(|n| p.channels_for(n).expect("assigned").len()).collect();
    assert_eq!(sizes, vec![13, 12, 11]);
}

#[test]
fn planner_returns_none_when_fleet_size_is_zero_or_exceeds_max_nodes() {
    assert!(plan(ChannelPool::Us, 0).is_none());
    assert!(plan(ChannelPool::Us, u8::try_from(MAX_NODES).expect("fits") + 1).is_none());
    assert!(plan_for(ChannelPool::Us, &[]).is_none());
    assert!(plan_for(ChannelPool::Us, &[Job::Wifi(Radio::DualBand); MAX_NODES + 1]).is_none());
}

#[test]
fn stagger_offset_ms_computes_slot_delay_when_given_node_index_and_count() {
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
fn wire_epoch_cycles_non_zero_u8_range_when_epoch_counter_increments() {
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
