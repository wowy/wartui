//! The P4's fleet planner: membership, the cut, the Bluetooth pick, and what goes to each node.
//!
//! The rig plays the nodes: it encodes their heartbeats with the real encoder, delivers what the
//! core sends, and has each node adopt an assignment's epoch the way the firmware does.

use std::collections::BTreeSet;

use wartui_proto::bus::{
    Assign, Board, Capabilities, DieTemp, Heartbeat, NODE_VERSION, Slot, Vdd, Version,
};
use wartui_proto::fleet::{
    BRIDGE_ID, Fleet, HEARTBEAT_TIMEOUT_MS, Report, ScanMode, SlotMsg, ToSlot, next_epoch,
    pick_bluetooth,
};
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, ChannelSet};
use wartui_proto::reset::ResetCause;

fn slot(byte: u8) -> Slot {
    Slot::from_u8(byte).expect("a slot")
}

/// A synthetic, locally administered address.
fn mac(n: u8) -> Mac {
    [0x02, 0, 0, 0, 0xF1, n]
}

fn beat_body(mac: Mac, version: Version, counter: u32, epoch: u8) -> [u8; 27] {
    Heartbeat {
        capabilities: Capabilities { version, ..Capabilities::here(Board::Esp32c5) },
        mac,
        counter,
        epoch,
        wifi_refused: 0,
        ble_refused: 0,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
        heap_free: 0,
    }
    .encode()
}

/// A node as the rig plays it.
#[derive(Clone, Copy)]
struct Node {
    mac: Mac,
    counter: u32,
    epoch: u8,
}

struct Rig {
    fleet: Fleet,
    now: u64,
    nodes: [Option<Node>; 14],
}

impl Rig {
    fn new() -> Self {
        Self { fleet: Fleet::new(ChannelPool::All), now: 1_000, nodes: [None; 14] }
    }

    fn started(bluetooth: bool) -> Self {
        let mut rig = Self::new();
        rig.fleet.start(bluetooth, rig.now);
        rig
    }

    fn plug(&mut self, byte: u8, n: u8) {
        self.plug_holding(byte, n, 0);
    }

    fn plug_holding(&mut self, byte: u8, n: u8, epoch: u8) {
        self.nodes[usize::from(byte)] = Some(Node { mac: mac(n), counter: 1, epoch });
    }

    fn unplug(&mut self, byte: u8) {
        self.nodes[usize::from(byte)] = None;
        let present =
            (0u8..14).filter(|b| self.nodes[usize::from(*b)].is_some()).fold(0, |m, b| m | 1 << b);
        self.fleet.on_presence(present, self.now);
    }

    fn node(&self, byte: u8) -> Node {
        self.nodes[usize::from(byte)].expect("a node there")
    }

    /// One heartbeat from the node at `byte`, then whatever the core sends.
    fn beat(&mut self, byte: u8) -> Vec<ToSlot> {
        let node = self.nodes[usize::from(byte)].as_mut().expect("a node there");
        node.counter += 1;
        let body = beat_body(node.mac, NODE_VERSION, node.counter, node.epoch);
        self.fleet.on_heartbeat(slot(byte), &body, self.now);
        self.drain()
    }

    fn drain(&mut self) -> Vec<ToSlot> {
        std::iter::from_fn(|| self.fleet.next_send(self.now)).collect()
    }

    /// Deliver `sends`, and have each node take what it was sent.
    fn deliver(&mut self, sends: &[ToSlot]) {
        for send in sends {
            self.fleet.on_delivered(send.id, self.now);
            if let Some(node) = self.nodes[usize::from(send.slot.as_u8())].as_mut() {
                node.epoch = match send.msg {
                    SlotMsg::Assign(assign) => assign.epoch,
                    SlotMsg::Stop => 0,
                };
            }
        }
    }

    /// Every node beats once, and what is sent is delivered.
    fn round(&mut self) -> Vec<ToSlot> {
        let mut all = Vec::new();
        for byte in 0..14 {
            if self.nodes[usize::from(byte)].is_some() {
                let sends = self.beat(byte);
                self.deliver(&sends);
                all.extend(sends);
            }
        }
        all
    }

    /// Rounds until the fleet sends nothing, returning everything sent.
    fn settle(&mut self) -> Vec<ToSlot> {
        let mut all = Vec::new();
        for _ in 0..5 {
            let sends = self.round();
            if sends.is_empty() {
                return all;
            }
            all.extend(sends);
        }
        panic!("the fleet never settled");
    }

    fn report(&self) -> Report {
        self.fleet.report()
    }
}

fn assigns(sends: &[ToSlot]) -> Vec<(u8, Assign)> {
    sends
        .iter()
        .filter_map(|send| match send.msg {
            SlotMsg::Assign(assign) => Some((send.slot.as_u8(), assign)),
            SlotMsg::Stop => None,
        })
        .collect()
}

fn stops(sends: &[ToSlot]) -> Vec<u8> {
    sends.iter().filter(|send| send.msg == SlotMsg::Stop).map(|send| send.slot.as_u8()).collect()
}

/// The shares as (slot, channels, ble).
fn shares(report: &Report) -> Vec<(u8, ChannelSet, bool)> {
    report.shares.iter().map(|share| (share.slot.as_u8(), share.channels, share.ble)).collect()
}

/// The shares' channels are disjoint and cover the pool.
fn assert_partition(report: &Report) {
    let mut seen = BTreeSet::new();
    for share in &report.shares {
        for index in share.channels.indices() {
            assert!(seen.insert(index), "channel index {index} dealt twice");
        }
    }
    let pool: BTreeSet<u8> = report.pool.channels().indices().collect();
    assert_eq!(seen, pool, "the shares cover the pool exactly");
}

/// Every Wi-Fi share is non-empty and every empty one carries the Bluetooth flag.
fn assert_no_empty_wifi(sends: &[ToSlot]) {
    for (byte, assign) in assigns(sends) {
        assert!(
            !assign.channels.is_empty() || assign.scan_ble(),
            "slot {byte} was sent an empty set without the Bluetooth flag"
        );
    }
}

// ---- pick_bluetooth ----

#[test]
fn pick_bluetooth_returns_none_when_fewer_than_two_slots() {
    assert_eq!(pick_bluetooth(None, 0), None);
    assert_eq!(pick_bluetooth(None, slot(3).bit()), None);
    assert_eq!(pick_bluetooth(Some(slot(3)), slot(3).bit()), None);
}

#[test]
fn pick_bluetooth_keeps_current_when_still_assignable() {
    let mask = slot(0).bit() | slot(4).bit() | slot(13).bit();
    assert_eq!(pick_bluetooth(Some(slot(4)), mask), Some(slot(4)));
}

#[test]
fn pick_bluetooth_takes_highest_slot_when_current_is_gone_or_none() {
    let mask = slot(0).bit() | slot(4).bit() | slot(13).bit();
    assert_eq!(pick_bluetooth(None, mask), Some(slot(13)));
    assert_eq!(pick_bluetooth(Some(slot(7)), mask), Some(slot(13)));
    assert_eq!(pick_bluetooth(None, slot(0).bit() | slot(1).bit()), Some(slot(1)));
}

// ---- epochs ----

#[test]
fn fleet_next_epoch_skips_counter_when_its_wire_epoch_is_held() {
    let mut last = 0;
    assert_eq!(next_epoch(&mut last, Some(1)), 2);
    assert_eq!(next_epoch(&mut last, None), 3);
    assert_eq!(next_epoch(&mut last, Some(0)), 4);
    // Counter 256 narrows to wire epoch 1 again.
    let mut last = 255;
    assert_eq!(next_epoch(&mut last, Some(1)), 257);
}

#[test]
fn fleet_skips_held_epoch_when_dealing_first_assignment() {
    let mut rig = Rig::started(false);
    rig.plug_holding(0, 1, 1);
    let sends = rig.beat(0);
    let sent = assigns(&sends);
    assert_eq!(sent.len(), 1);
    assert_ne!(sent[0].1.epoch, 1, "the node discards the epoch it holds");
    assert_ne!(sent[0].1.epoch, 0);
}

// ---- the cut ----

#[test]
fn fleet_partitions_whole_pool_when_nodes_join() {
    let mut rig = Rig::started(false);
    for (byte, n) in [(0, 1), (3, 2), (9, 3)] {
        rig.plug(byte, n);
        rig.settle();
        assert_partition(&rig.report());
    }
    let report = rig.report();
    let slots: Vec<u8> = report.shares.iter().map(|share| share.slot.as_u8()).collect();
    assert_eq!(slots, [0, 3, 9], "shares in slot order");
    assert!(report.shares.iter().all(|share| !share.ble && !share.channels.is_empty()));
    assert!(report.unreachable.is_empty());
}

#[test]
fn fleet_sends_assign_at_once_when_member_joins() {
    let mut rig = Rig::started(false);
    rig.plug(2, 1);
    let sends = rig.beat(2);
    assert_eq!(assigns(&sends).len(), 1);
    assert_eq!(sends[0].slot, slot(2));
    match sends[0].msg {
        SlotMsg::Assign(assign) => assert_eq!(assign.channels, ChannelPool::All.channels()),
        SlotMsg::Stop => panic!("a joiner is assigned"),
    }
}

#[test]
fn fleet_orders_plan_by_slot_when_nodes_join_out_of_order() {
    // The same fleet, joined in two orders, cuts the same shares per slot.
    let mut one = Rig::started(false);
    let mut two = Rig::started(false);
    for byte in [5, 1, 9] {
        one.plug(byte, byte);
    }
    for byte in [9, 5, 1] {
        two.plug(byte, byte);
    }
    one.settle();
    two.settle();
    assert_eq!(shares(&one.report()), shares(&two.report()));
}

#[test]
fn fleet_recuts_every_share_when_node_joins() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    let before = shares(&rig.report());
    rig.plug(2, 3);
    let sends = rig.settle();
    let after = shares(&rig.report());
    assert_ne!(before, after);
    assert_partition(&rig.report());
    let sent: BTreeSet<u8> = assigns(&sends).iter().map(|(byte, _)| *byte).collect();
    assert_eq!(sent, BTreeSet::from([0, 1, 2]), "every changed share is sent");
}

#[test]
fn fleet_redeals_share_when_presence_shows_slot_absent() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(4, 2);
    rig.settle();
    rig.unplug(4);
    let sends = rig.drain();
    assert_eq!(shares(&rig.report()), [(0, ChannelPool::All.channels(), false)]);
    assert!(sends.iter().all(|send| send.slot == slot(0)), "nothing to the absent slot");
    assert_eq!(assigns(&sends).len(), 1);
}

#[test]
fn fleet_keeps_slot_out_of_plan_when_present_without_heartbeat() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    rig.unplug(0);
    // The slot answers the bus again, but no heartbeat has said who is there.
    rig.fleet.on_presence(slot(0).bit(), rig.now);
    assert!(rig.drain().is_empty());
    assert!(rig.report().shares.is_empty());
    // A heartbeat brings it back.
    rig.plug(0, 1);
    assert_eq!(assigns(&rig.beat(0)).len(), 1);
}

#[test]
fn fleet_drops_member_when_heartbeat_times_out() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    let heard = rig.now;
    rig.now = heard + HEARTBEAT_TIMEOUT_MS - 1;
    rig.beat(0);
    rig.fleet.on_tick(rig.now);
    assert_eq!(rig.report().shares.len(), 2, "inside the timeout");
    rig.now = heard + HEARTBEAT_TIMEOUT_MS;
    rig.fleet.on_tick(rig.now);
    let sends = rig.drain();
    assert_eq!(shares(&rig.report()), [(0, ChannelPool::All.channels(), false)]);
    assert_eq!(assigns(&sends).iter().map(|(byte, _)| *byte).collect::<Vec<_>>(), [0]);
    // The silent node still answers the bus, so it is told to idle.
    assert_eq!(stops(&sends), [1]);
}

#[test]
fn fleet_recuts_when_pool_changes() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    rig.fleet.take_report();
    rig.fleet.set_pool(ChannelPool::Us, rig.now);
    let sends = rig.drain();
    assert_eq!(assigns(&sends).len(), 2);
    let report = rig.fleet.take_report().expect("a change");
    assert_eq!(report.pool, ChannelPool::Us);
    assert_partition(&report);
    assert_eq!(rig.fleet.pool(), ChannelPool::Us);
}

#[test]
fn fleet_keeps_plan_when_pool_set_unchanged() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    rig.fleet.take_report();
    rig.fleet.set_pool(ChannelPool::All, rig.now);
    assert!(rig.drain().is_empty());
    assert!(rig.fleet.take_report().is_none());
}

#[test]
fn fleet_keeps_epoch_and_sends_nothing_when_plan_settled() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    let report = rig.report();
    for _ in 0..3 {
        assert!(rig.round().is_empty());
    }
    assert_eq!(rig.report(), report);
}

#[test]
fn fleet_keeps_epoch_when_recut_leaves_share_unchanged() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    let before = rig.report().shares[0];
    // A lone node keeps Wi-Fi, so the mode change re-cuts to the same share.
    rig.fleet.start(true, rig.now);
    assert!(rig.drain().is_empty());
    assert_eq!(rig.report().shares[0], before);
    assert_eq!(rig.fleet.mode(), ScanMode::WifiBluetooth);
}

// ---- Bluetooth ----

#[test]
fn fleet_keeps_wifi_when_lone_node_in_bluetooth_mode() {
    let mut rig = Rig::started(true);
    rig.plug(3, 1);
    rig.settle();
    assert_eq!(rig.fleet.bluetooth(), None);
    assert_eq!(shares(&rig.report()), [(3, ChannelPool::All.channels(), false)]);
    assert!(rig.report().unreachable.is_empty());
}

#[test]
fn fleet_picks_bluetooth_when_second_node_joins() {
    let mut rig = Rig::started(true);
    rig.plug(3, 1);
    rig.settle();
    rig.plug(5, 2);
    let sends = rig.settle();
    assert_no_empty_wifi(&sends);
    assert_eq!(rig.fleet.bluetooth(), Some(slot(5)));
    assert_eq!(
        shares(&rig.report()),
        [(3, ChannelPool::All.channels(), false), (5, ChannelSet::empty(), true)]
    );
}

#[test]
fn fleet_keeps_one_bluetooth_scanner_when_fleet_grows() {
    let mut rig = Rig::started(true);
    let mut sent = Vec::new();
    for byte in 0..6 {
        rig.plug(byte, byte);
        sent.extend(rig.settle());
        let ble = rig.report().shares.iter().filter(|share| share.ble).count();
        assert_eq!(ble, usize::from(byte >= 1), "at most one scanner, with {} nodes", byte + 1);
        assert_partition(&rig.report());
    }
    assert_no_empty_wifi(&sent);
}

#[test]
fn fleet_keeps_bluetooth_pick_when_higher_slot_joins() {
    let mut rig = Rig::started(true);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    assert_eq!(rig.fleet.bluetooth(), Some(slot(1)));
    rig.plug(6, 3);
    rig.settle();
    assert_eq!(rig.fleet.bluetooth(), Some(slot(1)), "the pick is sticky");
}

#[test]
fn fleet_moves_bluetooth_pick_when_picked_node_leaves() {
    let mut rig = Rig::started(true);
    rig.plug(0, 1);
    rig.plug(6, 3);
    rig.settle();
    rig.plug(1, 2);
    rig.settle();
    assert_eq!(rig.fleet.bluetooth(), Some(slot(6)));
    rig.unplug(6);
    let sends = rig.drain();
    assert_eq!(rig.fleet.bluetooth(), Some(slot(1)), "the highest left");
    // Both ends of the move are re-cut: slot 1 to the scan, slot 0 to the whole pool.
    let sent = assigns(&sends);
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().any(|(byte, a)| *byte == 1 && a.scan_ble() && a.channels.is_empty()));
    assert!(sent.iter().any(|(byte, a)| *byte == 0 && a.channels == ChannelPool::All.channels()));
}

#[test]
fn fleet_returns_lone_node_to_wifi_when_other_node_leaves() {
    let mut rig = Rig::started(true);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    rig.unplug(0);
    let sends = rig.drain();
    assert_eq!(rig.fleet.bluetooth(), None);
    let sent = assigns(&sends);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, 1);
    assert!(!sent[0].1.scan_ble());
    assert_eq!(sent[0].1.channels, ChannelPool::All.channels());
}

#[test]
fn fleet_drops_bluetooth_when_mode_returns_to_wifi() {
    let mut rig = Rig::started(true);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    rig.fleet.start(false, rig.now);
    let sends = rig.drain();
    assert_eq!(rig.fleet.bluetooth(), None);
    assert!(assigns(&sends).iter().all(|(_, a)| !a.scan_ble()));
    rig.settle();
    assert_partition(&rig.report());
    assert!(rig.report().shares.iter().all(|share| !share.ble));
}

// ---- membership ----

#[test]
fn fleet_never_admits_node_when_version_is_undrivable() {
    let mut rig = Rig::started(false);
    let older = Version::new(NODE_VERSION.major, NODE_VERSION.minor.wrapping_sub(1), 0);
    rig.fleet.on_heartbeat(slot(0), &beat_body(mac(1), older, 1, 7), rig.now);
    assert!(rig.drain().is_empty(), "neither an Assign nor a Stop");
    assert!(rig.report().shares.is_empty());
    assert_eq!(rig.fleet.next_deadline_ms(), None);
}

#[test]
fn fleet_never_admits_node_when_heartbeat_does_not_decode() {
    let mut rig = Rig::started(false);
    let body = beat_body(mac(1), NODE_VERSION, 1, 7);
    rig.fleet.on_heartbeat(slot(0), &body[..20], rig.now);
    rig.fleet.on_heartbeat(slot(1), &body[..3], rig.now);
    assert!(rig.drain().is_empty());
    assert!(rig.report().shares.is_empty());
}

#[test]
fn fleet_withdraws_member_at_once_when_heartbeat_names_another_minor() {
    let mut rig = Rig::started(true);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    assert_eq!(rig.fleet.bluetooth(), Some(slot(1)));
    let newer = Version::new(NODE_VERSION.major, NODE_VERSION.minor + 1, 0);
    let node = rig.node(1);
    rig.fleet.on_heartbeat(slot(1), &beat_body(node.mac, newer, 9, node.epoch), rig.now);
    let sends = rig.drain();
    assert!(sends.iter().all(|send| send.slot == slot(0)), "nothing to the undrivable node");
    assert_eq!(rig.fleet.bluetooth(), None);
    assert_eq!(shares(&rig.report()), [(0, ChannelPool::All.channels(), false)]);
}

#[test]
fn fleet_reassigns_slot_when_another_mac_heartbeats_from_it() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    let epoch = rig.node(0).epoch;
    // Another node, holding by chance the epoch the slot's share carries.
    rig.plug_holding(0, 2, epoch);
    let sends = rig.beat(0);
    let sent = assigns(&sends);
    assert_eq!(sent.len(), 1, "the newcomer is told its share");
    assert_ne!(sent[0].1.epoch, epoch);
    assert_eq!(sent[0].1.channels, ChannelPool::All.channels());
}

#[test]
fn fleet_counts_heartbeat_as_presence_when_slot_not_yet_reported() {
    let mut rig = Rig::started(false);
    rig.fleet.on_presence(0, rig.now);
    rig.plug(4, 1);
    assert_eq!(assigns(&rig.beat(4)).len(), 1);
}

// ---- delivery ----

#[test]
fn fleet_sends_one_assignment_when_node_beats_twice_unanswered() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    assert_eq!(rig.beat(0).len(), 1);
    assert!(rig.beat(0).is_empty(), "one send in flight");
}

#[test]
fn fleet_resends_same_epoch_when_undelivered() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = rig.beat(0);
    rig.fleet.on_undelivered(first[0].id, rig.now);
    let again = rig.beat(0);
    assert_eq!(assigns(&again), assigns(&first));
    assert_ne!(again[0].id, first[0].id);
}

#[test]
fn fleet_queues_nothing_more_when_send_unanswered_long_after() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = rig.beat(0);
    // A backed-up downlink still holds it: a minute of heartbeats and ticks adds no duplicate.
    for _ in 0..60 {
        rig.now += 1_000;
        rig.fleet.on_tick(rig.now);
        assert!(rig.beat(0).is_empty(), "still in flight");
    }
    // Its answer comes at last, and the next heartbeat sends it again.
    rig.fleet.on_undelivered(first[0].id, rig.now);
    assert_eq!(assigns(&rig.beat(0)), assigns(&first));
}

#[test]
fn fleet_resends_when_delivered_epoch_not_adopted() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = rig.beat(0);
    // Delivered, but the node never took it.
    rig.fleet.on_delivered(first[0].id, rig.now);
    assert_eq!(assigns(&rig.beat(0)), assigns(&first));
}

#[test]
fn fleet_ignores_outcome_when_id_is_not_in_flight() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = rig.beat(0);
    rig.fleet.on_delivered(first[0].id ^ 1, rig.now);
    rig.fleet.on_undelivered(first[0].id.wrapping_add(7), rig.now);
    assert!(rig.beat(0).is_empty(), "the real send is still in flight");
}

#[test]
fn fleet_reissues_assignment_when_node_reboots() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = rig.settle();
    // A restart: the counter falls and the epoch is gone.
    let node = rig.nodes[0].as_mut().expect("a node");
    node.counter = 0;
    node.epoch = 0;
    let again = rig.beat(0);
    assert_eq!(assigns(&again), assigns(&first), "the same want, under the same epoch");
}

#[test]
fn fleet_reissues_assignment_when_counter_falls_with_send_in_flight() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    rig.fleet.stop(rig.now);
    rig.fleet.start(false, rig.now);
    let sends = rig.drain();
    let assign = assigns(&sends).last().copied().expect("an assign");
    // The node restarts with that Assign still in flight.
    let node = rig.nodes[0].as_mut().expect("a node");
    node.counter = 0;
    node.epoch = 0;
    let again = rig.beat(0);
    assert_eq!(assigns(&again), [assign], "a reboot re-sends over a send in flight");
}

#[test]
fn fleet_reissues_when_epoch_returns_to_zero_after_delivery() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = rig.settle();
    rig.nodes[0].as_mut().expect("a node").epoch = 0;
    assert_eq!(assigns(&rig.beat(0)), assigns(&first));
}

#[test]
fn fleet_reissues_under_new_epoch_when_rebooted_node_rejoins() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let first = assigns(&rig.settle())[0].1;
    rig.unplug(0);
    rig.plug(0, 1);
    let again = assigns(&rig.beat(0));
    assert_eq!(again.len(), 1);
    assert_ne!(again[0].1.epoch, first.epoch, "a departure ends the want");
    assert_eq!(again[0].1.channels, first.channels);
}

// ---- stop ----

#[test]
fn fleet_sends_stop_when_stopped_core_hears_held_epoch() {
    let mut rig = Rig::new();
    rig.plug_holding(0, 1, 9);
    let sends = rig.beat(0);
    assert_eq!(stops(&sends), [0]);
    assert!(assigns(&sends).is_empty());
    rig.deliver(&sends);
    assert!(rig.beat(0).is_empty(), "idle at epoch 0");
}

#[test]
fn fleet_sends_nothing_when_stopped_core_hears_epoch_zero() {
    let mut rig = Rig::new();
    rig.plug(0, 1);
    assert!(rig.beat(0).is_empty());
    assert!(rig.report().shares.is_empty());
}

#[test]
fn fleet_sends_assign_skipping_held_epoch_when_start_beats_stop() {
    let mut rig = Rig::new();
    rig.plug_holding(0, 1, 1);
    let body = beat_body(mac(1), NODE_VERSION, 2, 1);
    rig.fleet.on_heartbeat(slot(0), &body, rig.now);
    // The Stop is owed but not yet pulled when the start arrives.
    rig.fleet.start(false, rig.now);
    let sends = rig.drain();
    assert!(stops(&sends).is_empty(), "no Stop goes out");
    let sent = assigns(&sends);
    assert_eq!(sent.len(), 1);
    assert_ne!(sent[0].1.epoch, 1);
}

#[test]
fn fleet_idles_every_node_when_stopped() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(5, 2);
    rig.settle();
    rig.fleet.stop(rig.now);
    let sends = rig.drain();
    assert_eq!(stops(&sends), [0, 5]);
    assert!(assigns(&sends).is_empty());
    rig.deliver(&sends);
    assert!(rig.settle().is_empty());
    assert!(rig.report().shares.is_empty());
    assert_eq!(rig.report().mode, ScanMode::Stopped);
}

#[test]
fn fleet_sends_stop_when_stopped_with_assign_in_flight() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let assign = rig.beat(0);
    rig.fleet.stop(rig.now);
    let stop = rig.drain();
    assert_eq!(stops(&stop), [0], "delivery is in order, so the Stop lands last");
    assert_ne!(stop[0].id, assign[0].id);
    // The Assign's late answer is not the Stop's.
    rig.fleet.on_delivered(assign[0].id, rig.now);
    rig.fleet.on_undelivered(assign[0].id, rig.now);
    assert_eq!(rig.fleet.next_deadline_ms(), Some(rig.now + HEARTBEAT_TIMEOUT_MS));
    // The node took the Assign before the Stop: its heartbeat now shows the epoch, and the Stop
    // is still in flight, so nothing more goes.
    rig.deliver(&assign);
    assert!(rig.beat(0).is_empty());
    rig.deliver(&stop);
    assert!(rig.beat(0).is_empty());
}

#[test]
fn fleet_sends_stop_when_stopped_after_delivery_before_next_heartbeat() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    let assign = rig.beat(0);
    rig.deliver(&assign);
    // The last heartbeat still said epoch 0, but the node holds what was delivered. A stop that
    // waited for the next heartbeat would let a start right behind it skip the flush.
    rig.fleet.stop(rig.now);
    let stop = rig.drain();
    assert_eq!(stops(&stop), [0]);
    rig.deliver(&stop);
    assert!(rig.beat(0).is_empty());
}

#[test]
fn fleet_resends_stop_when_undelivered_and_heartbeat_holds_epoch() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    rig.fleet.stop(rig.now);
    let stop = rig.drain();
    rig.fleet.on_undelivered(stop[0].id, rig.now);
    let again = rig.beat(0);
    assert_eq!(stops(&again), [0]);
    rig.deliver(&again);
    assert!(rig.beat(0).is_empty());
}

#[test]
fn fleet_sends_nothing_when_node_reboots_while_stopped() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    rig.fleet.stop(rig.now);
    let stop = rig.drain();
    rig.fleet.on_undelivered(stop[0].id, rig.now);
    let node = rig.nodes[0].as_mut().expect("a node");
    node.counter = 0;
    node.epoch = 0;
    assert!(rig.beat(0).is_empty());
}

// ---- reports, ids, deadlines ----

#[test]
fn fleet_takes_report_once_when_state_changes() {
    let mut rig = Rig::new();
    assert!(rig.fleet.take_report().is_none(), "nothing has changed");
    rig.fleet.start(false, rig.now);
    let report = rig.fleet.take_report().expect("the mode changed");
    assert_eq!(report.mode, ScanMode::Wifi);
    assert!(report.shares.is_empty());
    assert!(rig.fleet.take_report().is_none());

    rig.plug(0, 1);
    rig.beat(0);
    let report = rig.fleet.take_report().expect("a share");
    assert_eq!(report, rig.report());
    assert_eq!(report.shares.len(), 1);
    assert!(rig.fleet.take_report().is_none());

    // Delivery and adoption change no share.
    rig.settle();
    assert!(rig.fleet.take_report().is_none());
    rig.fleet.start(false, rig.now);
    assert!(rig.fleet.take_report().is_none(), "the same mode again");

    rig.fleet.stop(rig.now);
    assert!(rig.fleet.take_report().is_some());
    assert!(rig.fleet.take_report().is_none());
}

#[test]
fn fleet_sends_ids_at_or_above_bridge_id_when_counter_wraps() {
    let mut rig = Rig::new();
    rig.plug(0, 1);
    rig.beat(0);
    let mut ids = Vec::new();
    // Each cycle sends an Assign, then a Stop over it.
    for _ in 0..=(1 << 14) {
        rig.fleet.start(false, rig.now);
        ids.extend(rig.drain().iter().map(|send| send.id));
        rig.fleet.stop(rig.now);
        ids.extend(rig.drain().iter().map(|send| send.id));
    }
    assert!(ids.len() > 1 << 15);
    assert!(ids.iter().all(|id| *id >= BRIDGE_ID));
    assert_eq!(ids[0], BRIDGE_ID);
    assert_eq!(ids[(1 << 15) - 1], u16::MAX);
    assert_eq!(ids[1 << 15], BRIDGE_ID, "wraps in 15 bits");
}

#[test]
fn fleet_reports_deadline_when_member_heartbeats() {
    let mut rig = Rig::started(false);
    assert_eq!(rig.fleet.next_deadline_ms(), None, "nothing pending");
    rig.plug(0, 1);
    let heard = rig.now;
    let sends = rig.beat(0);
    // A send in flight waits on its answer, not on the clock.
    assert_eq!(rig.fleet.next_deadline_ms(), Some(heard + HEARTBEAT_TIMEOUT_MS));
    rig.deliver(&sends);
    assert_eq!(rig.fleet.next_deadline_ms(), Some(heard + HEARTBEAT_TIMEOUT_MS));
    rig.now += 500;
    rig.beat(0);
    assert_eq!(rig.fleet.next_deadline_ms(), Some(rig.now + HEARTBEAT_TIMEOUT_MS));
    rig.unplug(0);
    assert_eq!(rig.fleet.next_deadline_ms(), None);
}

#[test]
fn fleet_starts_stopped_when_new() {
    const FLEET: Fleet = Fleet::new(ChannelPool::Eu);
    assert_eq!(FLEET.mode(), ScanMode::Stopped);
    assert_eq!(FLEET.pool(), ChannelPool::Eu);
    assert_eq!(FLEET.bluetooth(), None);
    assert!(FLEET.report().shares.is_empty());
}

// ---- members ----

/// The members `rig` lists now, as `(slot byte, mac)`.
fn members(rig: &Rig) -> Vec<(u8, Mac)> {
    rig.fleet.members().iter().map(|member| (member.slot.as_u8(), member.mac)).collect()
}

#[test]
fn fleet_lists_no_members_when_no_heartbeat_arrived() {
    let mut rig = Rig::started(false);
    rig.fleet.on_presence(0b11, rig.now);
    assert!(members(&rig).is_empty());
}

#[test]
fn fleet_lists_member_with_mac_when_slot_heartbeats() {
    let mut rig = Rig::started(false);
    rig.plug(3, 7);
    rig.beat(3);
    assert_eq!(members(&rig), [(3, mac(7))]);
}

#[test]
fn fleet_lists_members_when_stopped() {
    // The host files them before it starts the scan.
    let mut rig = Rig::new();
    rig.plug(0, 1);
    rig.beat(0);
    assert_eq!(rig.fleet.mode(), ScanMode::Stopped);
    assert_eq!(members(&rig), [(0, mac(1))]);
}

#[test]
fn fleet_lists_members_in_slot_order_when_nodes_join_out_of_order() {
    let mut rig = Rig::started(false);
    for (byte, n) in [(9, 1), (2, 2), (13, 3), (0, 4)] {
        rig.plug(byte, n);
        rig.beat(byte);
    }
    assert_eq!(members(&rig), [(0, mac(4)), (2, mac(2)), (9, mac(1)), (13, mac(3))]);
}

#[test]
fn fleet_lists_every_slot_when_fourteen_nodes_heartbeat() {
    let mut rig = Rig::started(false);
    for byte in 0..14 {
        rig.plug(byte, byte);
        rig.beat(byte);
    }
    assert_eq!(rig.fleet.members().len(), 14);
}

#[test]
fn fleet_unlists_member_when_presence_shows_slot_absent() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.plug(1, 2);
    rig.settle();
    rig.unplug(1);
    assert_eq!(members(&rig), [(0, mac(1))]);
}

#[test]
fn fleet_unlists_member_when_heartbeat_times_out() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.beat(0);
    rig.now += HEARTBEAT_TIMEOUT_MS - 1;
    rig.fleet.on_tick(rig.now);
    assert_eq!(members(&rig), [(0, mac(1))], "inside the timeout");
    rig.now += 1;
    rig.fleet.on_tick(rig.now);
    assert!(members(&rig).is_empty());
}

#[test]
fn fleet_unlists_member_when_heartbeat_names_another_minor() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.beat(0);
    let newer = Version::new(NODE_VERSION.major, NODE_VERSION.minor + 1, 0);
    rig.fleet.on_heartbeat(slot(0), &beat_body(mac(1), newer, 9, 0), rig.now);
    assert!(members(&rig).is_empty());
}

#[test]
fn fleet_lists_new_mac_when_another_node_heartbeats_from_slot() {
    let mut rig = Rig::started(false);
    rig.plug(0, 1);
    rig.settle();
    rig.plug(0, 2);
    rig.beat(0);
    assert_eq!(members(&rig), [(0, mac(2))]);
}
