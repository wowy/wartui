//! The channel-pool planner and its deals.
//!
//! The properties here are what stop a node being told to scan a channel the operator excluded,
//! or two nodes being given the same one while a third covers nothing. Every dealer passes one
//! contract suite, [`contract`], and each deal then has tests of its own: the even deal's say what
//! makes it even, and the weighted deal's pin its rules and the shares they give.

use std::collections::BTreeSet;

use wartui_proto::plan::{
    BUSY_FIVE_GHZ, ChannelPool, ChannelSet, Dealer, Even, IndexRun, Job, MAX_NODES,
    NUM_SCAN_CHANNELS, Plan, PlanSettings, Planner, SCAN_CHANNELS, SparseNodes, UNSUPPORTED_INDEX,
    Weighted, index_of, is_five_ghz, is_quiet, plan, plan_with,
};

const POOLS: [ChannelPool; 3] = [ChannelPool::Us, ChannelPool::Eu, ChannelPool::All];

const FLEET_SIZES: std::ops::RangeInclusive<u8> = 1..=MAX_NODES as u8;

/// The [`SCAN_CHANNELS`] indices of channels 1, 6 and 11.
const POPULAR: [u8; 3] = [0, 5, 10];

/// The set naming `channels`.
fn set(channels: &[u8]) -> ChannelSet {
    let mut set = ChannelSet::empty();
    for &channel in channels {
        set.insert(index_of(channel).expect("a table channel"));
    }
    set
}

/// The nodes holding `channel`, ascending.
fn holders(p: &Plan, channel: u8) -> Vec<u8> {
    let idx = index_of(channel).expect("a table channel");
    (0..p.node_count()).filter(|&n| p.channels_for(n).is_some_and(|s| s.contains(idx))).collect()
}

/// Every index the plan hands to anybody, with repeats kept so overlap shows.
fn covered(p: &Plan) -> Vec<u8> {
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
    // Slot order must not restart per run — two nodes sharing a slot would
    // report the same place in the fleet table.
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
fn planner_deals_1_6_and_11_first_then_ascending_round_robin_when_partitioning_pool() {
    // Position k of the deal order goes to node k % node_count, and the order is 1, 6 and 11,
    // then the rest of the pool ascending. Said out longhand because everything below is a
    // consequence of it, and a planner satisfying those by other means would pass the same tests.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let p = plan(pool, nodes).expect("valid fleet size");
            let flattened: Vec<u8> = pool.runs().iter().flat_map(|r| r.start..=r.end).collect();
            let order: Vec<u8> = POPULAR
                .into_iter()
                .chain(flattened.into_iter().filter(|idx| !POPULAR.contains(idx)))
                .collect();
            for (k, idx) in order.into_iter().enumerate() {
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
fn plan_encodes_fleet_metadata_into_assignment_when_generating_assign() {
    let p = plan(ChannelPool::Us, 5).expect("valid");
    for n in 0..5 {
        let assign = p.assign_for(n, 9, wartui_proto::bus::ASSIGN_FLAG_BLE).expect("assigned");
        assert_eq!(assign.epoch, 9);
        assert_eq!(assign.channels, p.channels_for(n).expect("assigned"));
        // Flags are the caller's: which node scans Bluetooth is the operator's
        // decision, and the plan is what acts on it.
        assert!(assign.scan_ble());
    }
}

#[test]
fn planner_assigns_empty_channel_set_when_node_holds_ble_flag() {
    // Counted, because `Plan::node_count` reports the fleet's shape rather than
    // a census of who is sniffing.
    let fleet = [Job::Bluetooth, Job::Wifi, Job::Wifi];
    let p = plan_with(ChannelPool::Us.channels(), &fleet, &Even).expect("a valid fleet");

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
    let explicit = plan_with(ChannelPool::Us.channels(), &[Job::Wifi; 2], &Even).expect("valid");
    assert_eq!(two, explicit, "the convenience route and the explicit one agree");

    let with_scanner = plan_with(
        ChannelPool::Us.channels(),
        &[Job::Wifi; 2].iter().copied().chain([Job::Bluetooth]).collect::<Vec<_>>(),
        &Even,
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
    let p = plan_with(ChannelPool::Us.channels(), &[Job::Bluetooth], &Even)
        .expect("one node is a valid fleet");
    assert_eq!(p.channels_for(0), Some(ChannelSet::empty()));
    assert_eq!(p.unreachable(), ChannelPool::Us.channels());
}

#[test]
fn plan_refuses_empty_channel_set_when_node_lacks_ble_flag() {
    // The one frame that must never exist: an empty mask without the flag tells a
    // node to scan nothing, and a node sent one parks while the host goes on
    // believing it is sweeping.
    let p = plan_with(ChannelPool::Us.channels(), &[Job::Bluetooth, Job::Wifi], &Even)
        .expect("a valid fleet");
    assert!(
        p.assign_for(0, 1, wartui_proto::bus::ASSIGN_FLAG_BLE)
            .is_some_and(|assign| assign.channels.is_empty())
    );
    assert!(p.assign_for(0, 1, 0).is_none(), "an empty mask without the flag is refused");
    assert!(p.assign_for(1, 1, 0).is_some(), "and a real share needs no flag");

    // A second Bluetooth slot is `None` either way: there is nothing to say to it, and
    // the flag does not invent something.
    let p = plan_with(ChannelPool::Us.channels(), &[Job::Bluetooth, Job::Bluetooth], &Even)
        .expect("a valid fleet");
    assert!(p.channels_for(1).is_none());
    assert!(p.assign_for(1, 1, wartui_proto::bus::ASSIGN_FLAG_BLE).is_none());
}

#[test]
fn planner_gives_every_wifi_slot_a_share_when_fleet_is_max_nodes_on_smallest_pool() {
    // What `FleetEngine::deal` relies on to send every sniffing node a share: no pool is
    // smaller than the largest fleet, so the deal never runs out of channels.
    let smallest =
        POOLS.into_iter().min_by_key(|pool| pool.channel_count()).expect("there are pools");
    assert!(usize::from(smallest.channel_count()) >= MAX_NODES);

    let p = plan_with(smallest.channels(), &[Job::Wifi; MAX_NODES], &Even).expect("a valid fleet");
    let nodes = u8::try_from(MAX_NODES).expect("fits");
    for node in 0..nodes {
        assert!(!p.channels_for(node).expect("assigned").is_empty(), "slot {node}");
    }

    let mut fleet = [Job::Wifi; MAX_NODES];
    fleet[0] = Job::Bluetooth;
    let p = plan_with(smallest.channels(), &fleet, &Even).expect("a valid fleet");
    for node in 1..nodes {
        assert!(!p.channels_for(node).expect("assigned").is_empty(), "slot {node}");
    }
}

#[test]
fn planner_produces_identical_plan_when_using_convenience_or_explicit_api() {
    // `plan` is `plan_with` the even deal, with every slot sniffing.
    for pool in POOLS {
        for nodes in FLEET_SIZES {
            let fleet = vec![Job::Wifi; usize::from(nodes)];
            assert_eq!(
                plan(pool, nodes),
                plan_with(pool.channels(), &fleet, &Even),
                "{pool:?}/{nodes}"
            );
        }
    }
}

#[test]
fn planner_returns_none_when_fleet_size_is_zero_or_exceeds_max_nodes() {
    assert!(plan(ChannelPool::Us, 0).is_none());
    assert!(plan(ChannelPool::Us, u8::try_from(MAX_NODES).expect("fits") + 1).is_none());
    assert!(plan_with(ChannelPool::Us.channels(), &[], &Even).is_none());
    assert!(plan_with(ChannelPool::Us.channels(), &[Job::Wifi; MAX_NODES + 1], &Even).is_none());
}

#[test]
fn planner_places_1_6_and_11_on_different_nodes_when_fleet_has_three_or_more() {
    // The busiest channels: a node holding two of them spends twice the dwell on the crowd.
    for pool in POOLS {
        for nodes in 3..=MAX_NODES as u8 {
            let p = plan(pool, nodes).expect("valid fleet size");
            let owners: BTreeSet<u8> =
                [1, 6, 11].into_iter().flat_map(|c| holders(&p, c)).collect();
            assert_eq!(owners.len(), 3, "{pool:?}/{nodes} nodes: 1, 6 and 11 share a node");
        }
    }
}

#[test]
fn planner_deals_only_listed_channels_when_given_a_list() {
    let listed = set(&[1, 6, 36, 149, 177]);
    for nodes in FLEET_SIZES {
        let fleet = vec![Job::Wifi; usize::from(nodes)];
        let p = plan_with(listed, &fleet, &Even).expect("valid fleet size");
        let seen: BTreeSet<u8> = covered(&p).into_iter().collect();
        let want: BTreeSet<u8> = listed.indices().collect();
        assert_eq!(seen, want, "{nodes} nodes dealt another set than the list");
    }
}

#[test]
fn planner_puts_1_6_and_11_on_nodes_0_1_and_2_when_dealing_us_pool_to_ten_nodes() {
    let p = plan(ChannelPool::Us, 10).expect("valid");
    assert_eq!(holders(&p, 1), [0]);
    assert_eq!(holders(&p, 6), [1]);
    assert_eq!(holders(&p, 11), [2]);
}

#[test]
fn planner_gives_node_0_channels_1_and_10_when_dealing_1_to_11_to_ten_nodes() {
    let p = plan_with(set(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]), &[Job::Wifi; 10], &Even)
        .expect("valid");
    assert_eq!(p.channels_for(0), Some(set(&[1, 10])));
    for node in 1..10 {
        assert_eq!(p.channels_for(node).expect("assigned").len(), 1, "node {node}");
    }
}

#[test]
fn planner_doubles_1_6_and_11_when_dealing_1_to_11_to_fourteen_nodes() {
    let p = plan_with(set(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]), &[Job::Wifi; 14], &Even)
        .expect("valid");
    for channel in 1..=11 {
        let want = if [1, 6, 11].contains(&channel) { 2 } else { 1 };
        assert_eq!(holders(&p, channel).len(), want, "channel {channel}");
    }
}

#[test]
fn planner_deals_1_6_and_11_to_four_three_and_three_nodes_when_dealing_them_to_ten() {
    let p = plan_with(set(&[1, 6, 11]), &[Job::Wifi; 10], &Even).expect("valid");
    assert_eq!(holders(&p, 1).len(), 4);
    assert_eq!(holders(&p, 6).len(), 3);
    assert_eq!(holders(&p, 11).len(), 3);
}

#[test]
fn planner_doubles_1_6_11_and_36_when_dealing_five_channels_to_nine_nodes() {
    let p = plan_with(set(&[1, 6, 11, 36, 40]), &[Job::Wifi; 9], &Even).expect("valid");
    for channel in [1, 6, 11, 36] {
        assert_eq!(holders(&p, channel).len(), 2, "channel {channel}");
    }
    assert_eq!(holders(&p, 40).len(), 1);
}

#[test]
fn planner_deals_36_to_three_nodes_and_40_to_two_when_dealing_them_to_five() {
    let p = plan_with(set(&[36, 40]), &[Job::Wifi; 5], &Even).expect("valid");
    assert_eq!(holders(&p, 36).len(), 3);
    assert_eq!(holders(&p, 40).len(), 2);
}

#[test]
fn planner_gives_every_wifi_slot_one_channel_when_channels_are_fewer_than_slots() {
    // Every node holds one channel, and the channels' node counts differ by at most one.
    let lists: [&[u8]; 4] = [&[1], &[1, 6, 11], &[36, 40, 44], &[1, 6, 11, 36, 40, 149]];
    for channels in lists {
        for nodes in channels.len() + 1..=MAX_NODES {
            let p = plan_with(set(channels), &vec![Job::Wifi; nodes], &Even).expect("valid");
            for node in 0..u8::try_from(nodes).expect("fits") {
                assert_eq!(p.channels_for(node).expect("assigned").len(), 1, "node {node}");
            }
            let counts: Vec<usize> = channels.iter().map(|&c| holders(&p, c).len()).collect();
            let (low, high) = (counts.iter().min(), counts.iter().max());
            assert!(
                high.zip(low).is_some_and(|(high, low)| high - low <= 1),
                "{channels:?} on {nodes} nodes: counts {counts:?}"
            );
        }
    }
}

#[test]
fn planner_deals_no_channel_to_bluetooth_slot_when_channels_are_fewer_than_slots() {
    let fleet = [Job::Wifi, Job::Wifi, Job::Bluetooth, Job::Wifi, Job::Wifi];
    let p = plan_with(set(&[1, 6]), &fleet, &Even).expect("valid");
    assert_eq!(p.bluetooth(), Some(2));
    assert_eq!(p.channels_for(2), Some(ChannelSet::empty()), "dealt nothing, and told so");
    for node in [0, 1, 3, 4] {
        assert_eq!(p.channels_for(node).expect("assigned").len(), 1, "node {node}");
    }
    assert_eq!(holders(&p, 1), [0, 3]);
    assert_eq!(holders(&p, 6), [1, 4]);
}

#[test]
fn wire_epoch_cycles_non_zero_u8_range_when_epoch_counter_increments() {
    use wartui_proto::bus::wire_epoch;

    // The host counts in a `u64`; the wire field is one byte and
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

// ---- the contract every dealer keeps ----

/// The fleets the contract deals to at `nodes` slots: all Wi-Fi, and with two or more, Bluetooth
/// on the first slot and on the last.
fn fleets(nodes: usize) -> Vec<Vec<Job>> {
    let mut fleets = vec![vec![Job::Wifi; nodes]];
    if nodes >= 2 {
        let mut first = vec![Job::Wifi; nodes];
        first[0] = Job::Bluetooth;
        let mut last = vec![Job::Wifi; nodes];
        last[nodes - 1] = Job::Bluetooth;
        fleets.extend([first, last]);
    }
    fleets
}

/// Slot `index` of a fleet, as the plan names it.
fn slot_of(index: usize) -> u8 {
    u8::try_from(index).expect("a fleet slot")
}

/// What `p` deals each slot of `fleet`, checked: every Wi-Fi slot holds a channel of `channels`,
/// every Bluetooth slot holds nothing, and the shares cover `channels` with nothing unreachable.
fn assert_dealt(p: &Plan, fleet: &[Job], channels: ChannelSet, case: &str) {
    for (index, job) in fleet.iter().enumerate() {
        let share = p.channels_for(slot_of(index));
        match job {
            Job::Wifi => {
                let share =
                    share.unwrap_or_else(|| panic!("{case}: Wi-Fi slot {index} dealt none"));
                assert!(!share.is_empty(), "{case}: Wi-Fi slot {index} holds nothing");
                assert_eq!(share.intersection(channels), share, "{case}: slot {index} off the set");
            }
            Job::Bluetooth => {
                assert_eq!(share, Some(ChannelSet::empty()), "{case}: Bluetooth slot {index}");
            }
        }
    }
    let seen: BTreeSet<u8> = covered(p).into_iter().collect();
    assert_eq!(seen, channels.indices().collect(), "{case}: the set is not covered exactly");
    assert!(p.unreachable().is_empty(), "{case}: something unreachable");
}

/// What every dealer must do, over every pool, 1 to 14 slots, with and without a Bluetooth slot,
/// and over short lists.
fn contract(dealer: &impl Dealer) {
    for pool in POOLS {
        let channels = pool.channels();
        for nodes in 1..=MAX_NODES {
            for fleet in fleets(nodes) {
                let case = format!("{pool:?} on {fleet:?}");
                let p = plan_with(channels, &fleet, dealer).expect("a valid fleet");
                assert_dealt(&p, &fleet, channels, &case);
                let indices = covered(&p);
                let unique: BTreeSet<u8> = indices.iter().copied().collect();
                assert_eq!(indices.len(), unique.len(), "{case}: a channel dealt twice");
                assert_eq!(plan_with(channels, &fleet, dealer), Some(p), "{case}: not repeatable");
                if fleet.iter().filter(|job| **job == Job::Wifi).count() >= 3 {
                    let owners: BTreeSet<u8> =
                        [1, 6, 11].into_iter().flat_map(|c| holders(&p, c)).collect();
                    assert_eq!(owners.len(), 3, "{case}: 1, 6 and 11 share a slot");
                }
            }
        }
    }

    // Fewer channels than slots, quiet channels only, and one channel.
    let us_quiet: Vec<u8> = ChannelPool::Us
        .channels()
        .indices()
        .filter(|idx| is_quiet(*idx))
        .map(|idx| SCAN_CHANNELS[usize::from(idx)])
        .collect();
    let lists: [&[u8]; 7] = [
        &[1],
        &[100],
        &[1, 6, 11],
        &[36, 40, 44],
        &[1, 6, 11, 36, 40, 149],
        &[52, 56, 60],
        &us_quiet,
    ];
    for list in lists {
        for nodes in 1..=MAX_NODES {
            for fleet in fleets(nodes) {
                let p = plan_with(set(list), &fleet, dealer).expect("a valid fleet");
                assert_dealt(&p, &fleet, set(list), &format!("{list:?} on {fleet:?}"));
            }
        }
    }
}

#[test]
fn planner_keeps_contract_when_dealing_even() {
    contract(&Even);
}

#[test]
fn weighted_keeps_contract_when_sparse_nodes_take_every_value() {
    for sparse_nodes in SparseNodes::ALL {
        contract(&Weighted { sparse_nodes });
    }
}

#[test]
fn planner_keeps_contract_when_plan_settings_name_either_planner() {
    for planner in Planner::ALL {
        for sparse_nodes in SparseNodes::ALL {
            contract(&PlanSettings { planner, sparse_nodes });
        }
    }
}

// ---- what `plan_with` holds a dealer to ----

/// Deals evenly, then adds index `.0` to every share.
struct Adds(u8);

impl Dealer for Adds {
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]) {
        Even.deal(channels, shares);
        for share in shares {
            share.insert(self.0);
        }
    }
}

/// Deals evenly, leaving index `.0` out.
struct Drops(u8);

impl Dealer for Drops {
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]) {
        let mut without = ChannelSet::empty();
        for idx in channels.indices().filter(|idx| *idx != self.0) {
            without.insert(idx);
        }
        Even.deal(without, shares);
    }
}

/// Deals evenly to every share but the first.
struct SkipsFirst;

impl Dealer for SkipsFirst {
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]) {
        Even.deal(channels, &mut shares[1..]);
    }
}

/// Fails the test if it is asked to deal.
struct Refuses;

impl Dealer for Refuses {
    fn deal(&self, _: ChannelSet, _: &mut [ChannelSet]) {
        panic!("dealt to a fleet with no Wi-Fi slot");
    }
}

#[test]
fn planner_masks_share_to_set_when_dealer_names_channel_outside_it() {
    let outside = index_of(165).expect("a table channel");
    let channels = set(&[1, 6, 11, 36]);
    let p = plan_with(channels, &[Job::Wifi; 3], &Adds(outside)).expect("a valid fleet");
    for node in 0..3 {
        assert!(!p.channels_for(node).expect("assigned").contains(outside), "node {node}");
    }
    assert_eq!(covered(&p).into_iter().collect::<BTreeSet<_>>(), channels.indices().collect());
    assert!(p.unreachable().is_empty());
}

#[test]
fn planner_reports_channel_unreachable_when_dealer_deals_it_to_no_one() {
    let dropped = index_of(36).expect("a table channel");
    let p = plan_with(ChannelPool::Us.channels(), &[Job::Wifi; 4], &Drops(dropped)).expect("valid");
    assert_eq!(p.unreachable(), set(&[36]));
    assert!(holders(&p, 36).is_empty());
}

#[test]
fn planner_gives_slot_no_channels_when_dealer_leaves_its_share_empty() {
    let p = plan_with(ChannelPool::Us.channels(), &[Job::Wifi; 3], &SkipsFirst).expect("valid");
    assert_eq!(p.channels_for(0), None);
    assert!(p.assign_for(0, 1, 0).is_none(), "no assignment for an empty share");
    assert!(p.channels_for(1).is_some() && p.channels_for(2).is_some());
}

#[test]
fn planner_skips_dealer_when_fleet_has_no_wifi_slot() {
    let p = plan_with(ChannelPool::Us.channels(), &[Job::Bluetooth], &Refuses).expect("valid");
    assert_eq!(p.unreachable(), ChannelPool::Us.channels());
}

// ---- the weighted deal ----

fn weighted(sparse_nodes: SparseNodes) -> Weighted {
    Weighted { sparse_nodes }
}

/// The quiet channels `channels` holds.
fn quiet_of(channels: ChannelSet) -> ChannelSet {
    let mut quiet = ChannelSet::empty();
    for idx in channels.indices().filter(|idx| is_quiet(*idx)) {
        quiet.insert(idx);
    }
    quiet
}

/// Whether `share` holds only quiet channels.
fn all_quiet(share: ChannelSet) -> bool {
    quiet_of(share) == share
}

/// The shares of `p` for `nodes` all-Wi-Fi slots.
fn shares_of(p: &Plan, nodes: usize) -> Vec<ChannelSet> {
    (0..nodes).map(|n| p.channels_for(slot_of(n)).expect("assigned")).collect()
}

/// A weighted plan for `nodes` all-Wi-Fi slots on `pool`.
fn weighted_plan(pool: ChannelPool, nodes: usize, sparse_nodes: SparseNodes) -> Plan {
    plan_with(pool.channels(), &vec![Job::Wifi; nodes], &weighted(sparse_nodes)).expect("valid")
}

/// How many sparse nodes `p` has among `nodes` slots: the trailing shares holding only quiet
/// channels.
fn sparse_count(p: &Plan, nodes: usize) -> usize {
    shares_of(p, nodes).iter().rev().take_while(|share| all_quiet(**share)).count()
}

#[test]
fn weighted_deals_issue_table_when_us_pool_meets_ten_wifi_slots_on_auto() {
    let p = weighted_plan(ChannelPool::Us, 10, SparseNodes::AUTO);
    let want: [&[u8]; 10] = [
        &[1, 157],
        &[6, 100],
        &[11, 149],
        &[2, 8, 153],
        &[3, 9, 48],
        &[4, 10, 40],
        &[5, 36, 161],
        &[7, 44, 112],
        &[52, 60, 104, 116, 124, 132, 140, 165],
        &[56, 64, 108, 120, 128, 136, 144],
    ];
    for (node, channels) in want.into_iter().enumerate() {
        assert_eq!(p.channels_for(slot_of(node)), Some(set(channels)), "N{}", node + 1);
    }
}

#[test]
fn weighted_deals_as_even_when_fleet_has_three_wifi_slots_or_fewer() {
    for pool in POOLS {
        for sparse_nodes in SparseNodes::ALL {
            for nodes in 1..=3 {
                for fleet in fleets(nodes) {
                    assert_eq!(
                        plan_with(pool.channels(), &fleet, &weighted(sparse_nodes)),
                        plan_with(pool.channels(), &fleet, &Even),
                        "{pool:?} on {fleet:?}, sparse nodes {sparse_nodes}"
                    );
                }
            }
        }
    }
    // Four Wi-Fi slots behind a Bluetooth one weight, though five slots are in the fleet.
    let mut fleet = [Job::Wifi; 5];
    fleet[4] = Job::Bluetooth;
    let p =
        plan_with(ChannelPool::Us.channels(), &fleet, &weighted(SparseNodes::AUTO)).expect("valid");
    assert!(all_quiet(p.channels_for(3).expect("assigned")));
}

#[test]
fn weighted_puts_only_quiet_channels_on_highest_wifi_slots_when_weighting() {
    for pool in POOLS {
        let quiet = quiet_of(pool.channels());
        for nodes in 4..=MAX_NODES {
            for sparse_nodes in SparseNodes::ALL {
                let case = format!("{pool:?}/{nodes}/{sparse_nodes}");
                let p = weighted_plan(pool, nodes, sparse_nodes);
                let shares = shares_of(&p, nodes);
                let k = sparse_nodes.count_for(nodes).min(nodes - 3).min(quiet.len() as usize);
                assert!(k >= 1, "{case}: every pool weights from four slots");
                let (busy, sparse) = shares.split_at(nodes - k);
                assert!(sparse.iter().all(|share| all_quiet(*share)), "{case}: a busy channel");
                assert!(busy.iter().all(|share| quiet_of(*share).is_empty()), "{case}: quiet");
                let held = sparse.iter().fold(0, |all, share| all | share.bits());
                assert_eq!(ChannelSet::from_bits(held), quiet, "{case}: quiet not all sparse");
            }
        }
    }
}

#[test]
fn weighted_balances_busy_shares_within_one_when_weighting() {
    for pool in POOLS {
        for nodes in 4..=MAX_NODES {
            for sparse_nodes in SparseNodes::ALL {
                let case = format!("{pool:?}/{nodes}/{sparse_nodes}");
                let p = weighted_plan(pool, nodes, sparse_nodes);
                let shares = shares_of(&p, nodes);
                let busy = &shares[..nodes - sparse_count(&p, nodes)];
                let lens: Vec<u32> = busy.iter().map(|share| share.len()).collect();
                let low = lens.iter().min().expect("three busy shares at least");
                let high = lens.iter().max().expect("three busy shares at least");
                assert!(high - low <= 1, "{case}: busy shares ran {low}..={high}");
                // A share holding 1, 6 or 11 is never longer than one holding none of them.
                let holds_popular =
                    |share: &ChannelSet| POPULAR.into_iter().any(|idx| share.contains(idx));
                let popular = busy.iter().filter(|s| holds_popular(s)).map(|s| s.len()).max();
                let rest = busy.iter().filter(|s| !holds_popular(s)).map(|s| s.len()).min();
                if let (Some(popular), Some(rest)) = (popular, rest) {
                    assert!(popular <= rest, "{case}: 1, 6 or 11 on a longer share");
                }
            }
        }
    }
}

#[test]
fn weighted_gives_every_busy_share_both_bands_when_each_band_has_a_channel_per_busy_share() {
    let mut checked = 0;
    for pool in POOLS {
        let channels = pool.channels();
        let two_four = channels.indices().filter(|idx| !is_five_ghz(*idx)).count();
        let five = BUSY_FIVE_GHZ.into_iter().filter(|idx| channels.contains(*idx)).count();
        for nodes in 4..=MAX_NODES {
            for sparse_nodes in SparseNodes::ALL {
                let p = weighted_plan(pool, nodes, sparse_nodes);
                let b = nodes - sparse_count(&p, nodes);
                if two_four < b || five < b {
                    continue;
                }
                checked += 1;
                for (node, share) in shares_of(&p, b).into_iter().enumerate() {
                    let bands: BTreeSet<bool> = share.indices().map(is_five_ghz).collect();
                    assert_eq!(bands.len(), 2, "{pool:?}/{nodes}/{sparse_nodes}: node {node}");
                }
            }
        }
    }
    assert!(checked > 0, "some case has a channel of each band per busy share");
}

#[test]
fn weighted_never_makes_bluetooth_slot_a_sparse_node_when_fleet_has_one() {
    let alone = shares_of(&weighted_plan(ChannelPool::Us, 10, SparseNodes::AUTO), 10);
    // On the highest slot, where the fleet planner picks it, and on the lowest.
    for bluetooth in [10, 0] {
        let mut fleet = vec![Job::Wifi; 11];
        fleet[bluetooth] = Job::Bluetooth;
        let p = plan_with(ChannelPool::Us.channels(), &fleet, &weighted(SparseNodes::AUTO))
            .expect("valid");
        assert_eq!(p.channels_for(slot_of(bluetooth)), Some(ChannelSet::empty()));
        let wifi: Vec<ChannelSet> = (0..11)
            .filter(|slot| *slot != bluetooth)
            .map(|slot| p.channels_for(slot_of(slot)).expect("assigned"))
            .collect();
        assert_eq!(wifi, alone, "Bluetooth on slot {bluetooth}: the Wi-Fi shares as alone");
    }
}

#[test]
fn weighted_weights_own_quiet_channels_when_pool_is_eu_or_all() {
    for pool in [ChannelPool::Eu, ChannelPool::All] {
        let p = weighted_plan(pool, 10, SparseNodes::AUTO);
        let sparse = &shares_of(&p, 10)[8..];
        let held = ChannelSet::from_bits(sparse.iter().fold(0, |all, share| all | share.bits()));
        assert_eq!(held, quiet_of(pool.channels()), "{pool:?}");
    }
    let p = weighted_plan(ChannelPool::All, 10, SparseNodes::AUTO);
    for channel in [169, 173, 177] {
        assert!(holders(&p, channel).iter().all(|node| *node >= 8), "channel {channel}");
    }
    // No 149 to 161 on `eu`, so its busy 5 GHz list is six channels.
    let eu = ChannelPool::Eu.channels();
    assert_eq!(BUSY_FIVE_GHZ.into_iter().filter(|idx| eu.contains(*idx)).count(), 6);
}

#[test]
fn weighted_asks_for_one_two_and_three_sparse_nodes_when_auto_meets_4_to_14_slots() {
    for (nodes, want) in [(4, 1), (5, 1), (6, 2), (10, 2), (11, 3), (14, 3)] {
        assert_eq!(SparseNodes::AUTO.count_for(nodes), want, "{nodes} slots");
    }
    for nodes in 4..=MAX_NODES {
        let want = if nodes <= 5 {
            1
        } else if nodes <= 10 {
            2
        } else {
            3
        };
        let p = weighted_plan(ChannelPool::Us, nodes, SparseNodes::AUTO);
        assert_eq!(sparse_count(&p, nodes), want, "{nodes} slots");
    }
}

#[test]
fn weighted_leaves_three_busy_shares_when_four_sparse_nodes_meet_five_slots() {
    let four = SparseNodes::fixed(4).expect("in range");
    for pool in POOLS {
        let p = weighted_plan(pool, 5, four);
        assert_eq!(sparse_count(&p, 5), 2, "{pool:?}");
        for node in 0..3 {
            assert!(quiet_of(p.channels_for(node).expect("assigned")).is_empty(), "{pool:?}");
        }
    }
}

#[test]
fn weighted_deals_as_even_when_channels_are_1_to_11() {
    let channels = set(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    for nodes in 1..=MAX_NODES {
        for sparse_nodes in SparseNodes::ALL {
            let fleet = vec![Job::Wifi; nodes];
            assert_eq!(
                plan_with(channels, &fleet, &weighted(sparse_nodes)),
                plan_with(channels, &fleet, &Even),
                "{nodes} slots, sparse nodes {sparse_nodes}"
            );
        }
    }
}

// ---- the settings ----

#[test]
fn planner_deals_as_named_planner_when_dealing_plan_settings() {
    for pool in POOLS {
        for nodes in 1..=MAX_NODES {
            let fleet = vec![Job::Wifi; nodes];
            for sparse_nodes in SparseNodes::ALL {
                let even = PlanSettings { planner: Planner::Even, sparse_nodes };
                let weighted_settings = PlanSettings { planner: Planner::Weighted, sparse_nodes };
                assert_eq!(
                    plan_with(pool.channels(), &fleet, &even),
                    plan_with(pool.channels(), &fleet, &Even)
                );
                assert_eq!(
                    plan_with(pool.channels(), &fleet, &weighted_settings),
                    plan_with(pool.channels(), &fleet, &weighted(sparse_nodes))
                );
            }
        }
    }
}

#[test]
fn planner_defaults_to_even_and_auto_when_nothing_is_set() {
    assert_eq!(PlanSettings::default(), PlanSettings::DEFAULT);
    assert_eq!(PlanSettings::DEFAULT.planner, Planner::Even);
    assert_eq!(PlanSettings::DEFAULT.sparse_nodes, SparseNodes::AUTO);
    assert_eq!(Planner::default(), Planner::Even);
    assert_eq!(SparseNodes::default(), SparseNodes::AUTO);
}

#[test]
fn planner_names_even_and_weighted_when_displayed() {
    let names: Vec<String> = Planner::ALL.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["even", "weighted"]);
}

#[test]
fn sparse_nodes_refuses_5_when_built_or_decoded() {
    assert_eq!(SparseNodes::fixed(5), None);
    assert_eq!(SparseNodes::fixed(0), None, "0 is auto, not a fixed count");
    assert_eq!(SparseNodes::from_byte(5), None);
    assert!(SparseNodes::try_from(5).is_err());
    assert!(postcard::from_bytes::<SparseNodes>(&[5]).is_err());
}

#[test]
fn sparse_nodes_round_trips_byte_when_encoded() {
    let names: Vec<String> = SparseNodes::ALL.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["auto", "1", "2", "3", "4"]);
    for (byte, sparse) in (0u8..).zip(SparseNodes::ALL) {
        assert_eq!(sparse.byte(), byte);
        assert_eq!(SparseNodes::from_byte(byte), Some(sparse));
        assert_eq!(u8::from(sparse), byte);
        let mut buf = [0; 4];
        let bytes = postcard::to_slice(&sparse, &mut buf).expect("encodes");
        assert_eq!(bytes, [byte]);
        assert_eq!(postcard::from_bytes::<SparseNodes>(bytes).expect("decodes"), sparse);
    }
    for n in 1..=SparseNodes::MAX {
        assert_eq!(SparseNodes::fixed(n).map(SparseNodes::byte), Some(n));
        assert_eq!(SparseNodes::fixed(n).map(|s| s.count_for(14)), Some(usize::from(n)));
    }
}
