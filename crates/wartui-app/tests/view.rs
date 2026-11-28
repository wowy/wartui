//! What a front end shows of a snapshot: every rule `View::of` holds, on the typed values.

mod common;

use std::time::Duration;

use chrono::{NaiveDate, NaiveTime};
use wartui_app::settings::Link;
use wartui_app::view::{
    Answers, Augmentation, BridgeView, ChannelBar, FanShown, Fault, FirmwareShown, FixShown,
    HostShown, LinkFault, LinkFaultKind, NodeRow, NodeStatus, Note, PlannerShown, Planning,
    PowerAnswered, PowerCycle, ReceiverPort, ReceiverStatus, ReceiverSummary, RejectsFrom,
    SatelliteShown, ScanShown, SdAnswered, SdLogShown, SdShown, SettingsShown, Severity,
    ShareShown, SkyDate, SlotShown, View,
};
use wartui_bridge::{BridgeCore, DisconnectCause};
use wartui_core::engine::{NodeView, PowerAnswer, SdAnswer, Snapshot};
use wartui_core::gps::{GpsCounters, GpsStatus, GpsView};
use wartui_core::nmea::{Constellation, FixMode, Satellite, Sky};
use wartui_core::position::{Fix, PositionSource};
use wartui_proto::bus::{
    Board, Capabilities, DieTemp, NODE_VERSION, NodeBoard, Slot, Vdd, Version,
};
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::fleet::{ScanMode, Share};
use wartui_proto::link::{
    BRIDGE_VERSION, Central, End, PowerOutcome, SdFormatOutcome, SdLog, SdState, SdStatus,
    Standing, Trial,
};
use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun, PlanSettings, Planner, SparseNodes};
use wartui_proto::reset::ResetCause;

use common::{node, receiver, running, snapshot};

/// `snapshot` as a run over USB shows it.
fn usb(snapshot: &Snapshot) -> View {
    View::of(snapshot, Link::Usb)
}

/// The view of `node` alone in a running capture.
fn row_of(node: NodeView) -> NodeRow {
    usb(&running(vec![node])).nodes.remove(0)
}

/// A bonded central on an encrypted link, at `mtu`.
fn central(mtu: u16) -> Central {
    Central { encrypted: true, bonded: true, mtu, phy: 2, interval_us: 30_000 }
}

/// A running capture whose bridge reports `central` connected to its C6.
fn with_central(central: Option<Central>) -> Snapshot {
    let mut snapshot = running(Vec::new());
    if let Some(status) = &mut snapshot.bridge_status {
        status.bluetooth.central = central;
    }
    snapshot
}

/// A running capture whose bridge reports its fan at `pct` and `rpm`.
fn with_fan(pct: u8, rpm: Option<u16>) -> Snapshot {
    let mut snapshot = running(Vec::new());
    let duty = FanDuty::new(pct).expect("in range");
    if let Some(status) = &mut snapshot.bridge_status {
        status.fan = Some(FanStats { duty, rpm });
    }
    snapshot
}

/// A node of a minor version behind this build's: never alive, never assignable.
fn outdated(last: u8) -> NodeView {
    let mut view = node(last);
    let version = wartui_bridge::sim::minor_behind(NODE_VERSION);
    view.state.capabilities = Some(Capabilities { version, ..Capabilities::here(Board::Esp32c5) });
    view.alive = false;
    view.assignable = false;
    view
}

/// `view` given `channels` under epoch 3, or the Bluetooth scan with none, adopted or not.
fn give(view: &mut NodeView, channels: ChannelSet, adopted: bool) {
    let slot = view.state.slot.expect("a slot");
    view.share = Some(Share { slot, epoch: 3, ble: channels.is_empty(), channels });
    view.adopted = adopted;
}

#[test]
fn view_names_usb_host_when_run_is_over_usb() {
    assert_eq!(usb(&running(Vec::new())).link.host, HostShown::Usb { bluetooth_waits: false });
}

#[test]
fn view_reports_bluetooth_waiting_when_bonded_central_is_connected_over_usb() {
    let waiting = with_central(Some(central(182)));
    assert_eq!(usb(&waiting).link.host, HostShown::Usb { bluetooth_waits: true });
    // The C6 drops a central with no bond, so it is not waiting for USB.
    let unbonded = with_central(Some(Central { bonded: false, ..central(182) }));
    assert_eq!(usb(&unbonded).link.host, HostShown::Usb { bluetooth_waits: false });
}

#[test]
fn view_reports_mtu_when_run_is_over_bluetooth_and_bridge_reported_it() {
    let reported = View::of(&with_central(Some(central(182))), Link::Bluetooth);
    assert_eq!(reported.link.host, HostShown::Bluetooth { mtu: Some(182) });
    let unreported = View::of(&with_central(None), Link::Bluetooth);
    assert_eq!(unreported.link.host, HostShown::Bluetooth { mtu: None });
}

#[test]
fn view_ignores_bridges_central_when_link_is_down() {
    // The last report describes a host that may have gone.
    let mut snapshot = with_central(Some(central(182)));
    snapshot.link_up = false;
    assert!(!usb(&snapshot).link.up);
    assert_eq!(usb(&snapshot).link.host, HostShown::Usb { bluetooth_waits: false });
    let bluetooth = View::of(&snapshot, Link::Bluetooth);
    assert_eq!(bluetooth.link.host, HostShown::Bluetooth { mtu: None });
}

#[test]
fn view_names_bridge_when_it_has_announced_itself() {
    let shown = usb(&running(Vec::new())).bridge;
    let bridge = BridgeView {
        mac: "02:00:5E:10:9D:24".to_owned(),
        mac_tail: "9D:24".to_owned(),
        firmware: "0.1.0".to_owned(),
    };
    assert_eq!(shown, Some(bridge));
    assert_eq!(usb(&snapshot(None)).bridge, None);
}

#[test]
fn view_shows_fan_when_bridge_reports_one() {
    assert_eq!(
        usb(&with_fan(40, Some(1830))).fan,
        Some(FanShown { duty_pct: 40, rpm: Some(1830) })
    );
    assert_eq!(usb(&with_fan(40, None)).fan, Some(FanShown { duty_pct: 40, rpm: None }));
}

#[test]
fn view_shows_no_fan_when_link_is_down_or_bridge_has_no_carrier() {
    let mut down = with_fan(40, Some(1830));
    down.link_up = false;
    assert_eq!(usb(&down).fan, None);
    assert_eq!(usb(&running(Vec::new())).fan, None, "no carrier");
    let mut unreported = with_fan(40, Some(1830));
    unreported.bridge_status = None;
    assert_eq!(usb(&unreported).fan, None);
}

#[test]
fn view_reports_scan_mode_when_bridge_reports_one() {
    let scan = |mode, slot| {
        let mut snapshot = running(Vec::new());
        snapshot.scan = mode;
        snapshot.ble_slot = slot;
        usb(&snapshot).fleet.scan
    };
    assert_eq!(scan(None, None), ScanShown::Unknown);
    assert_eq!(scan(Some(ScanMode::Stopped), None), ScanShown::Stopped);
    assert_eq!(scan(Some(ScanMode::Wifi), Slot::from_u8(6)), ScanShown::Wifi);
    assert_eq!(
        scan(Some(ScanMode::WifiBluetooth), Slot::from_u8(6)),
        ScanShown::WifiBluetooth { node: Some("N7".to_owned()) },
        "the slot by its label on the carrier"
    );
    assert_eq!(scan(Some(ScanMode::WifiBluetooth), None), ScanShown::WifiBluetooth { node: None });
}

#[test]
fn view_carries_pool_when_bridge_reports_one() {
    let mut snapshot = running(Vec::new());
    assert_eq!(usb(&snapshot).fleet.pool, None);
    snapshot.pool = Some(ChannelPool::Us);
    assert_eq!(usb(&snapshot).fleet.pool, Some(ChannelPool::Us));
}

#[test]
fn view_reports_planning_shares_when_nodes_hold_them() {
    let mut held = node(0x84);
    give(&mut held, ChannelPool::Us.channels(), true);
    let snapshot = running(vec![held, node(0x85)]);
    assert_eq!(usb(&snapshot).fleet.planning, Planning::Shares { held: 1, of: 2 });
}

#[test]
fn view_reports_planning_none_usable_when_alive_nodes_hold_no_share() {
    let snapshot = running(vec![node(0x84), node(0x85), node(0x86)]);
    assert_eq!(usb(&snapshot).fleet.planning, Planning::NoneUsable { alive: 3 });
}

#[test]
fn view_reports_planning_waiting_when_no_node_is_heard() {
    assert_eq!(usb(&running(Vec::new())).fleet.planning, Planning::WaitingForNodes);
}

#[test]
fn view_reports_planning_undrivable_when_only_nodes_heard_run_another_minor() {
    let planning = |older, newer| {
        let mut snapshot = running(vec![outdated(0x84), outdated(0x85)]);
        snapshot.needs_update = older;
        snapshot.host_behind = newer;
        usb(&snapshot).fleet.planning
    };
    assert_eq!(planning(1, 0), Planning::NeedUpdate { nodes: 1 });
    assert_eq!(planning(2, 0), Planning::NeedUpdate { nodes: 2 });
    assert_eq!(planning(0, 2), Planning::HostBehind { nodes: 2 });
    assert_eq!(planning(1, 2), Planning::Undrivable { nodes: 3 });
    assert_eq!(planning(0, 0), Planning::AllSilent, "heard, and none heartbeats now");
}

#[test]
fn view_ranks_node_status_in_precedence_order_when_several_hold() {
    // Every condition at once, then each removed in turn, reveals the next.
    let mut view = outdated(0x84);
    view.starting = true;
    view.state.last_heartbeat = None;
    view.on_bus = false;
    view.state.reboots = 2;
    let status = |view: &NodeView| row_of(view.clone()).status;
    assert_eq!(status(&view), NodeStatus::NeedsUpdate);
    let caps = Capabilities::here(Board::Esp32c5);
    let minor = NODE_VERSION.minor.checked_add(1).expect("a minor after this one");
    view.state.capabilities =
        Some(Capabilities { version: Version { minor, ..NODE_VERSION }, ..caps });
    assert_eq!(status(&view), NodeStatus::HostBehind);
    view.state.capabilities = Some(caps);
    assert_eq!(status(&view), NodeStatus::Starting);
    view.starting = false;
    assert_eq!(status(&view), NodeStatus::NoHeartbeat);
    view.state.last_heartbeat = Some(view.state.last_seen);
    assert_eq!(status(&view), NodeStatus::OffBus);
    view.on_bus = true;
    let slot = view.state.slot.take();
    assert_eq!(status(&view), NodeStatus::NoSlot);
    view.state.slot = slot;
    assert_eq!(status(&view), NodeStatus::Stale);
    view.assignable = true;
    assert_eq!(status(&view), NodeStatus::Rebooted { times: 2 });
    view.state.reboots = 0;
    assert_eq!(status(&view), NodeStatus::Alive);
}

#[test]
fn view_names_node_stale_when_it_is_unassignable() {
    let mut stale = node(0x84);
    stale.assignable = false;
    let row = row_of(stale);
    assert_eq!((row.status, row.severity), (NodeStatus::Stale, Severity::Warning));
}

#[test]
fn view_rates_severity_by_status_when_readings_are_in_range() {
    let mut rebooted = node(0x84);
    rebooted.state.reboots = 1;
    let mut starting = node(0x84);
    starting.starting = true;
    starting.state.last_heartbeat = None;
    let mut unslotted = node(0x84);
    unslotted.state.slot = None;
    for (view, want) in [
        (outdated(0x84), Severity::Fault),
        (starting, Severity::Ok),
        (unslotted, Severity::Warning),
        (rebooted, Severity::Notice),
        (node(0x84), Severity::Ok),
    ] {
        assert_eq!(row_of(view.clone()).severity, want, "{view:?}");
    }
}

#[test]
fn view_raises_severity_to_warning_only_from_ok_when_node_is_hot_or_supply_drops() {
    let hot = |mut view: NodeView| {
        view.state.die_temp = DieTemp::from_c(70);
        view
    };
    let dropped = |mut view: NodeView| {
        view.state.vdd_baseline = Vdd::from_mv(3304);
        view.state.vdd = Vdd::from_mv(3148);
        view
    };
    let mut rebooted = node(0x84);
    rebooted.state.reboots = 1;
    let mut starting = node(0x84);
    starting.starting = true;

    let alive_hot = row_of(hot(node(0x84)));
    assert_eq!((alive_hot.status, alive_hot.severity), (NodeStatus::Alive, Severity::Warning));
    assert!(alive_hot.hot && !alive_hot.supply_dropped);
    let alive_dropped = row_of(dropped(node(0x84)));
    assert_eq!(alive_dropped.severity, Severity::Warning);
    assert!(alive_dropped.supply_dropped && !alive_dropped.hot);
    assert_eq!(row_of(hot(starting)).severity, Severity::Warning, "starting is Ok too");
    assert_eq!(row_of(dropped(rebooted)).severity, Severity::Notice, "the reboot says more");
    assert_eq!(row_of(hot(outdated(0x84))).severity, Severity::Fault);
    let mut steady = node(0x84);
    steady.state.die_temp = DieTemp::from_c(69);
    assert_eq!(row_of(steady).severity, Severity::Ok, "one degree short of hot");
}

#[test]
fn view_shows_share_pending_when_node_has_not_adopted_it() {
    let share = |channels, adopted| {
        let mut view = node(0x84);
        give(&mut view, channels, adopted);
        row_of(view).share
    };
    let us = ChannelSet::from_run(IndexRun::new(14, 38));
    let channels: Vec<u8> = vec![
        36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
        149, 153, 157, 161, 165,
    ];
    assert_eq!(
        share(us, false),
        ShareShown::Channels { channels: channels.clone(), pending: true }
    );
    assert_eq!(share(us, true), ShareShown::Channels { channels, pending: false });
    assert_eq!(share(ChannelSet::empty(), false), ShareShown::Bluetooth { pending: true });
    assert_eq!(share(ChannelSet::empty(), true), ShareShown::Bluetooth { pending: false });
}

#[test]
fn view_shows_node_idle_when_scan_is_stopped_and_unassigned_while_it_scans() {
    let mut snapshot = running(vec![node(0x84)]);
    snapshot.scan = Some(ScanMode::Stopped);
    assert_eq!(usb(&snapshot).nodes[0].share, ShareShown::Idle);
    for scan in [None, Some(ScanMode::Wifi), Some(ScanMode::WifiBluetooth)] {
        snapshot.scan = scan;
        assert_eq!(usb(&snapshot).nodes[0].share, ShareShown::Unassigned, "{scan:?}");
    }
}

#[test]
fn view_refuses_power_cycle_in_order_when_node_cannot_be_reached() {
    let cycle = |view: NodeView, board: NodeBoard, up: bool| {
        let mut snapshot = running(vec![view]);
        snapshot.link_up = up;
        if let Some(bridge) = &mut snapshot.bridge {
            bridge.node_board = board;
        }
        usb(&snapshot).nodes[0].power_cycle
    };
    let mut unheard = node(0x84);
    unheard.state.slot = None;
    unheard.state.capabilities = None;
    let mut unslotted = node(0x84);
    unslotted.state.slot = None;

    assert_eq!(cycle(unheard.clone(), NodeBoard::NoCarrier, false), PowerCycle::NotConnected);
    assert_eq!(cycle(unheard.clone(), NodeBoard::NoCarrier, true), PowerCycle::NoCarrier);
    assert_eq!(cycle(unheard, NodeBoard::Carrier14, true), PowerCycle::NotHeard);
    assert_eq!(cycle(unslotted, NodeBoard::Carrier14, true), PowerCycle::LostSlot);
    // One that has stopped heartbeating is what a power-cycle is for.
    let mut silent = node(0x84);
    silent.alive = false;
    silent.assignable = false;
    assert_eq!(cycle(silent, NodeBoard::Carrier14, true), PowerCycle::Allowed);
}

#[test]
fn view_describes_node_when_it_has_heartbeated_from_a_slot() {
    let mut view = node(0x84);
    view.state.die_temp = DieTemp::from_c(41);
    view.state.batches_lost = 3;
    let row = row_of(view);
    assert_eq!(row.mac, "02:00:5E:10:57:84");
    assert_eq!(row.mac_tail, "57:84");
    let slot = SlotShown { index: 6, label: "N7".to_owned() };
    assert_eq!(Slot::from_u8(slot.index), Slot::new(0, 6), "A7");
    assert_eq!(row.slot, Some(slot));
    assert_eq!(row.name(), "N7");
    assert_eq!(row.board, Some(Board::Esp32c5));
    let current = FirmwareShown { version: NODE_VERSION.to_string(), update_to: None };
    assert_eq!(row.firmware, Some(current));
    assert_eq!((row.sightings, row.batches_lost, row.die_temp_c), (340, 3, Some(41)));
}

#[test]
fn view_names_node_by_address_tail_when_node_has_no_slot() {
    let mut view = node(0x84);
    view.state.slot = None;
    let row = row_of(view);
    assert_eq!((row.name(), &row.slot), ("57:84", &None));
}

#[test]
fn view_names_slot_by_bus_when_bridge_has_no_carrier() {
    // Byte 7 is B1, the first slot of bus B.
    let mut snapshot = running(vec![node(0x85)]);
    if let Some(bridge) = &mut snapshot.bridge {
        bridge.node_board = NodeBoard::NoCarrier;
    }
    assert_eq!(usb(&snapshot).nodes[0].name(), "B1");
}

#[test]
fn view_names_update_when_node_runs_older_minor() {
    let row = row_of(outdated(0x84));
    let older = wartui_bridge::sim::minor_behind(NODE_VERSION);
    let firmware =
        FirmwareShown { version: older.to_string(), update_to: Some(NODE_VERSION.to_string()) };
    assert_eq!(row.firmware, Some(firmware));
}

#[test]
fn view_shows_no_board_or_firmware_when_node_has_not_heartbeated() {
    let mut view = node(0x84);
    view.state.capabilities = None;
    let row = row_of(view);
    assert_eq!((row.board, row.firmware, row.die_temp_c), (None, None, None));
}

#[test]
fn view_keeps_snapshot_order_when_listing_nodes() {
    let mut slotless = node(0x80);
    slotless.state.slot = None;
    let snapshot = running(vec![node(0x86), node(0x84), slotless]);
    let names: Vec<String> = usb(&snapshot).nodes.iter().map(|row| row.name().to_owned()).collect();
    assert_eq!(names, ["N9", "N7", "57:80"]);
}

#[test]
fn view_carries_totals_when_capture_counts_them() {
    let mut snapshot = running(Vec::new());
    let c = &mut snapshot.counters;
    (c.frames, c.observations, c.wifi_observations, c.ble_observations) = (900, 800, 640, 160);
    (c.heartbeats, c.batches_lost, c.duplicate_batches) = (90, 7, 3);
    snapshot.store.written = 799;
    (snapshot.unique_wifi_aps, snapshot.unique_ble_aps) = (32, 8);
    let t = usb(&snapshot).totals;
    assert_eq!(
        (t.frames, t.sightings, t.wifi_sightings, t.bluetooth_sightings),
        (900, 800, 640, 160)
    );
    assert_eq!((t.heartbeats, t.stored, t.batches_lost, t.duplicate_batches), (90, 799, 7, 3));
    assert_eq!((t.unique_wifi, t.unique_bluetooth), (32, 8));
}

/// The channel numbers the view charts under `pool`.
fn charted(pool: Option<ChannelPool>) -> Vec<ChannelBar> {
    let mut snapshot = running(Vec::new());
    snapshot.pool = pool;
    snapshot.wifi_by_channel[0] = 280;
    snapshot.wifi_by_channel[41] = 5;
    usb(&snapshot).chart
}

#[test]
fn view_charts_every_channel_of_all_but_14_when_bridge_has_reported_no_pool() {
    let bars = charted(None);
    assert_eq!(
        bars,
        charted(Some(ChannelPool::All)),
        "All, the pool a bridge with none stored holds"
    );
    let channels: Vec<u8> = bars.iter().map(|bar| bar.channel).collect();
    assert_eq!(channels.len(), 41);
    assert!(!channels.contains(&14));
    // Every bar after 13 is a 5 GHz channel, in table order.
    assert_eq!(&channels[12..14], &[13, 36]);
    assert_eq!(channels.last(), Some(&177));
    assert_eq!(bars[0], ChannelBar { channel: 1, sightings: 280, starts_band: true });
    assert_eq!(bars[40], ChannelBar { channel: 177, sightings: 5, starts_band: false });
    let starts: Vec<u8> =
        bars.iter().filter(|bar| bar.starts_band).map(|bar| bar.channel).collect();
    assert_eq!(starts, [1, 36, 100, 149]);
}

#[test]
fn view_charts_only_pool_channels_when_pool_is_us() {
    let charted: Vec<u8> = charted(Some(ChannelPool::Us)).iter().map(|b| b.channel).collect();
    assert_eq!(charted.len(), 36);
    for hidden in [12, 13, 14, 169, 173, 177] {
        assert!(!charted.contains(&hidden), "{hidden}: {charted:?}");
    }
    assert_eq!(charted.last(), Some(&165));
}

#[test]
fn view_charts_only_pool_channels_when_pool_is_eu() {
    let charted: Vec<u8> = charted(Some(ChannelPool::Eu)).iter().map(|b| b.channel).collect();
    assert_eq!(charted.len(), 32);
    for hidden in [14, 144, 149, 153, 157, 161, 165, 169, 173, 177] {
        assert!(!charted.contains(&hidden), "{hidden}: {charted:?}");
    }
    assert_eq!(charted.last(), Some(&140));
}

#[test]
fn view_reports_position_when_rows_have_one() {
    let mut snapshot = running(Vec::new());
    let shown = usb(&snapshot).position;
    assert_eq!((shown.located, shown.source), (false, PositionSource::None));
    snapshot.position = Fix {
        lat: Some(37.7749),
        lon: Some(-122.4194),
        source: PositionSource::Static,
        ..Fix::none()
    };
    let shown = usb(&snapshot).position;
    assert_eq!((shown.located, shown.source), (true, PositionSource::Static));
}

#[test]
fn view_carries_fix_wait_when_run_holds_its_scan() {
    let mut snapshot = running(Vec::new());
    assert!(!usb(&snapshot).awaiting_fix);
    snapshot.awaiting_fix = true;
    assert!(usb(&snapshot).awaiting_fix);
}

#[test]
fn view_carries_clock_when_snapshot_is_taken() {
    let snapshot = running(Vec::new());
    let view = usb(&snapshot);
    assert_eq!((view.now_ms, view.started_at_ms), (snapshot.now_ms, snapshot.started_at_ms));
}

/// A running capture whose host receiver is in `status`, its rows placed by `source`.
fn with_gps(status: GpsStatus, source: PositionSource) -> Snapshot {
    let mut snapshot = running(Vec::new());
    snapshot.position.source = source;
    snapshot.gps = Some(receiver(status, &snapshot));
    snapshot
}

/// What the header says the receiver does for `snapshot`.
fn summary(snapshot: &Snapshot) -> ReceiverSummary {
    usb(snapshot).receiver.expect("a receiver").summary
}

#[test]
fn view_shows_no_receiver_when_none_is_read() {
    assert_eq!(usb(&running(Vec::new())).receiver, None);
}

#[test]
fn view_summarizes_receiver_failure_first_when_source_is_still_gps() {
    // The seconds after an unplug: the rows still carry its fix, and the port is gone.
    let failed = GpsStatus::Failed("/dev/cu.gps: Device not configured".to_owned());
    let reason = "/dev/cu.gps: Device not configured".to_owned();
    assert_eq!(summary(&with_gps(failed, PositionSource::Gps)), ReceiverSummary::Failed { reason });
}

#[test]
fn view_summarizes_receiver_positioning_when_rows_take_its_fix() {
    let fixed = with_gps(GpsStatus::Fixed { satellites: Some(8) }, PositionSource::Gps);
    assert_eq!(summary(&fixed), ReceiverSummary::Positioning { satellites: Some(8) });
    let unsaid = with_gps(GpsStatus::Fixed { satellites: None }, PositionSource::Gps);
    assert_eq!(summary(&unsaid), ReceiverSummary::Positioning { satellites: None });
    let searching = with_gps(GpsStatus::Searching, PositionSource::Gps);
    assert_eq!(summary(&searching), ReceiverSummary::Positioning { satellites: None });
}

#[test]
fn view_summarizes_receiver_by_status_when_rows_take_another_source() {
    let scanning = GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 38_400 };
    for (status, want) in [
        (GpsStatus::Connecting, ReceiverSummary::Connecting),
        (scanning, ReceiverSummary::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 38_400 }),
        (GpsStatus::Searching, ReceiverSummary::Searching),
        (GpsStatus::Fixed { satellites: Some(8) }, ReceiverSummary::StaleFix),
        (GpsStatus::NoReceiver, ReceiverSummary::NoReceiver),
    ] {
        assert_eq!(summary(&with_gps(status, PositionSource::Static)), want);
    }
}

#[test]
fn view_reports_receiver_status_and_no_sky_when_receiver_is_not_read() {
    let scanning = GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 9_600 };
    let failed = GpsStatus::Failed("/dev/ttyACM1 is no longer attached".to_owned());
    for (status, want) in [
        (GpsStatus::Connecting, ReceiverStatus::Connecting),
        (scanning, ReceiverStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 9_600 }),
        (GpsStatus::NoReceiver, ReceiverStatus::NoReceiver),
        (
            failed,
            ReceiverStatus::Failed { reason: "/dev/ttyACM1 is no longer attached".to_owned() },
        ),
    ] {
        let shown = usb(&with_gps(status, PositionSource::None)).receiver.expect("a receiver");
        assert_eq!(shown.status, want);
        assert_eq!(shown.sky, None, "a lost receiver leaves no satellites behind");
    }
}

#[test]
fn view_reports_receiver_reading_with_sky_when_sentences_arrive() {
    for status in [GpsStatus::Searching, GpsStatus::Fixed { satellites: Some(8) }] {
        let shown = usb(&with_gps(status, PositionSource::None)).receiver.expect("a receiver");
        let reading = ReceiverStatus::Reading { fix: FixShown::Searching, silent_secs: None };
        assert_eq!(shown.status, reading);
        assert!(shown.sky.is_some());
    }
}

#[test]
fn view_reports_receiver_silent_only_when_past_its_max_age() {
    let mut snapshot = with_gps(GpsStatus::Searching, PositionSource::None);
    snapshot.now_ms += 12_000;
    let status = |snapshot: &Snapshot| usb(snapshot).receiver.expect("a receiver").status;
    snapshot.gps.as_mut().expect("a receiver").max_age = Duration::from_secs(30);
    assert_eq!(
        status(&snapshot),
        ReceiverStatus::Reading { fix: FixShown::Searching, silent_secs: None }
    );
    snapshot.gps.as_mut().expect("a receiver").max_age = Duration::from_secs(5);
    let silent = ReceiverStatus::Reading { fix: FixShown::Searching, silent_secs: Some(12) };
    assert_eq!(status(&snapshot), silent);
}

/// What `sky` says of its fix, heard or not.
fn fix(sky: Sky, heard: bool) -> FixShown {
    let mut snapshot = with_gps(GpsStatus::Searching, PositionSource::None);
    let gps = snapshot.gps.as_mut().expect("a receiver");
    gps.sky = sky;
    if !heard {
        gps.heard_ms = None;
    }
    match usb(&snapshot).receiver.expect("a receiver").status {
        ReceiverStatus::Reading { fix, .. } => fix,
        other => panic!("not reading: {other:?}"),
    }
}

#[test]
fn view_reports_fix_searching_when_gga_reports_none_after_3d_mode() {
    // The two arrive separately, and a lost lock reaches `GGA` while the last `GSA` says 3D.
    let lost = Sky { quality: Some(0), mode: Some(FixMode::ThreeD), ..Sky::default() };
    assert_eq!(fix(lost, true), FixShown::Searching);
    let none = Sky { quality: Some(1), mode: Some(FixMode::NoFix), ..Sky::default() };
    assert_eq!(fix(none, true), FixShown::Searching);
}

#[test]
fn view_reports_fix_mode_and_augmentation_when_receiver_has_a_fix() {
    let sky = |quality, mode| Sky { quality: Some(quality), mode, ..Sky::default() };
    assert_eq!(
        fix(sky(2, Some(FixMode::ThreeD)), true),
        FixShown::ThreeD { augmented: Augmentation::Dgps }
    );
    assert_eq!(
        fix(sky(1, Some(FixMode::TwoD)), true),
        FixShown::TwoD { augmented: Augmentation::None }
    );
    for (quality, augmented) in [
        (1, Augmentation::None),
        (2, Augmentation::Dgps),
        (3, Augmentation::None),
        (4, Augmentation::RtkFixed),
        (5, Augmentation::RtkFloat),
        (6, Augmentation::DeadReckoning),
    ] {
        // A receiver that sends no `GSA` still says it has a fix in `GGA`.
        assert_eq!(fix(sky(quality, None), true), FixShown::Unspecified { augmented });
    }
}

#[test]
fn view_reports_fix_waiting_for_sentences_when_none_has_parsed() {
    assert_eq!(fix(Sky::default(), false), FixShown::WaitingForSentences);
    assert_eq!(fix(Sky::default(), true), FixShown::Searching);
}

#[test]
fn view_names_receiver_port_when_search_has_settled_or_receiver_is_bridges() {
    let port = |via_bridge, settled: Option<(String, u32)>| {
        let mut snapshot = with_gps(GpsStatus::Searching, PositionSource::None);
        let gps = snapshot.gps.as_mut().expect("a receiver");
        (gps.via_bridge, gps.settled) = (via_bridge, settled);
        usb(&snapshot).receiver.expect("a receiver").port
    };
    let settled = Some(("/dev/ttyACM1".to_owned(), 9_600));
    assert_eq!(port(true, None), ReceiverPort::Bridge);
    assert_eq!(
        port(false, settled),
        ReceiverPort::Host { port: "/dev/ttyACM1".to_owned(), baud: 9_600 }
    );
    assert_eq!(port(false, None), ReceiverPort::Unsettled);
}

#[test]
fn view_reports_receiver_sky_when_receiver_is_read() {
    let mut snapshot = with_gps(GpsStatus::Fixed { satellites: Some(8) }, PositionSource::Gps);
    let gps = snapshot.gps.as_mut().expect("a receiver");
    gps.counters = GpsCounters { sentences: 1_234, fixes: 400, rejected: 2 };
    gps.sky = Sky {
        quality: Some(2),
        mode: Some(FixMode::ThreeD),
        used_count: Some(8),
        time: NaiveTime::from_hms_opt(15, 40, 12),
        date: NaiveDate::from_ymd_opt(2026, 10, 10),
        lat: Some(48.1173),
        lon: Some(11.5167),
        alt: Some(545.4),
        hdop: Some(0.9),
        speed_kmh: Some(42.3),
        satellites: Vec::new(),
    };
    let shown = usb(&snapshot).receiver.expect("a receiver");
    assert_eq!((shown.sentences, shown.rejected), (1_234, 2));
    let sky = shown.sky.expect("read");
    assert_eq!(sky.used, Some(8));
    assert_eq!(sky.date, Some(SkyDate { year: 2026, month: 10, day: 10 }));
    assert_eq!(sky.time_secs, Some(15 * 3600 + 40 * 60 + 12));
    assert_eq!((sky.lat, sky.lon, sky.alt_m), (Some(48.1173), Some(11.5167), Some(545.4)));
    assert_eq!((sky.speed_kmh, sky.hdop), (Some(42.3), Some(0.9)));
    assert!(sky.accuracy_m.is_some_and(|m| (m - 4.5).abs() < 1e-9), "{:?}", sky.accuracy_m);
}

#[test]
fn view_sorts_satellites_used_first_then_by_snr_when_receiver_is_read() {
    let sat = |system, prn, snr, used| Satellite { system, prn, snr, used };
    let mut snapshot = with_gps(GpsStatus::Searching, PositionSource::None);
    snapshot.gps.as_mut().expect("a receiver").sky.satellites = vec![
        sat(Constellation::Gps, 1, Some(20), false),
        sat(Constellation::Gps, 2, None, true),
        sat(Constellation::Gps, 3, Some(40), true),
        sat(Constellation::Galileo, 4, Some(45), false),
        sat(Constellation::Glonass, 4, Some(45), false),
        sat(Constellation::Gps, 5, None, false),
    ];
    let shown: Vec<SatelliteShown> =
        usb(&snapshot).receiver.and_then(|r| r.sky).expect("read").satellites;
    let order: Vec<(Constellation, u16)> = shown.iter().map(|s| (s.system, s.prn)).collect();
    assert_eq!(
        order,
        [
            (Constellation::Gps, 3),
            (Constellation::Gps, 2),
            (Constellation::Glonass, 4),
            (Constellation::Galileo, 4),
            (Constellation::Gps, 1),
            (Constellation::Gps, 5),
        ],
        "used, strongest, then system and PRN so the list holds still"
    );
    assert_eq!(
        shown[0],
        SatelliteShown { system: Constellation::Gps, prn: 3, snr: Some(40), used: true }
    );
}

#[test]
fn view_lists_nothing_when_run_is_clean() {
    let view = usb(&running(vec![node(0x84)]));
    assert!(view.faults.is_empty(), "{:?}", view.faults);
    assert!(view.notes.is_empty(), "{:?}", view.notes);
}

/// A capture with the link down, for `reason` and `cause`.
fn down(reason: Option<&str>, cause: Option<DisconnectCause>) -> View {
    let mut snapshot = running(Vec::new());
    snapshot.link_up = false;
    snapshot.link_error = reason.map(str::to_owned);
    snapshot.link_cause = cause;
    usb(&snapshot)
}

#[test]
fn view_lists_link_fault_first_when_link_is_down() {
    let mut snapshot = running(Vec::new());
    snapshot.link_up = false;
    snapshot.link_error = Some("unplugged".to_owned());
    snapshot.link_cause = Some(DisconnectCause::Other);
    snapshot.store.dropped = 7;
    let faults = usb(&snapshot).faults;
    let link = LinkFault { kind: LinkFaultKind::Other, detail: "unplugged".to_owned() };
    assert_eq!(faults, [Fault::LinkDown(link), Fault::StoreDropped { rows: 7 }]);
}

#[test]
fn view_lists_link_fault_of_kind_other_when_transport_names_no_cause() {
    for cause in [Some(DisconnectCause::Other), None] {
        let view = down(Some("no bridge found"), cause);
        let link = LinkFault { kind: LinkFaultKind::Other, detail: "no bridge found".to_owned() };
        assert_eq!(view.faults, [Fault::LinkDown(link)], "{cause:?}");
        assert!(view.notes.is_empty());
    }
}

#[test]
fn view_lists_link_fault_of_its_kind_when_transport_names_a_cause() {
    let kind = |cause| match &down(Some("why"), Some(cause)).faults[..] {
        [Fault::LinkDown(link)] => link.kind,
        other => panic!("{other:?}"),
    };
    assert_eq!(kind(DisconnectCause::NotBonded), LinkFaultKind::NotBonded);
    assert_eq!(kind(DisconnectCause::MaybeNotBonded), LinkFaultKind::MaybeNotBonded);
    assert_eq!(kind(DisconnectCause::RadioOff), LinkFaultKind::RadioOff);
    assert_eq!(kind(DisconnectCause::NotAuthorized), LinkFaultKind::NotAuthorized);
    assert_eq!(kind(DisconnectCause::StaleBond), LinkFaultKind::StaleBond);
    assert_eq!(kind(DisconnectCause::NotAdvertising), LinkFaultKind::NotAdvertising);
    assert_eq!(kind(DisconnectCause::SeveralAdvertising), LinkFaultKind::SeveralAdvertising);
}

#[test]
fn view_lists_no_link_fault_when_link_is_up_or_no_reason_is_held() {
    assert!(down(None, Some(DisconnectCause::Other)).faults.is_empty());
    let mut up = running(Vec::new());
    up.link_error = Some("from before".to_owned());
    up.link_cause = Some(DisconnectCause::Other);
    assert!(usb(&up).faults.is_empty());
    assert!(usb(&up).notes.is_empty());
}

#[test]
fn view_notes_usb_taking_bridge_and_not_fault_when_usb_takes_it() {
    for reason in [Some("the USB host took the bridge"), None] {
        let view = down(reason, Some(DisconnectCause::UsbTookBridge));
        assert_eq!(view.notes, [Note::UsbHoldsBridge], "{reason:?}");
        assert!(view.faults.is_empty(), "{:?}", view.faults);
    }
}

#[test]
fn view_notes_pairing_and_ready_waits_and_not_faults_when_transport_waits() {
    let notice = "waiting for 02:00:5E:10:44:C0 to answer";
    let pairing = down(Some(notice), Some(DisconnectCause::AwaitingPairing));
    assert_eq!(pairing.notes, [Note::WaitingOnPairing { notice: notice.to_owned() }]);
    assert!(pairing.faults.is_empty());
    let ready = down(Some(notice), Some(DisconnectCause::AwaitingReady));
    assert_eq!(ready.notes, [Note::WaitingOnReady { notice: notice.to_owned() }]);
    assert!(ready.faults.is_empty());
    // A wait with nothing to say is neither.
    let silent = down(None, Some(DisconnectCause::AwaitingPairing));
    assert!(silent.notes.is_empty() && silent.faults.is_empty());
}

#[test]
fn view_notes_buffer_full_first_when_pending_buffers_turned_sightings_away() {
    let mut snapshot = running(Vec::new());
    snapshot.counters.ble_refused = 4;
    assert_eq!(usb(&snapshot).notes, [Note::BufferFull { wifi: 0, bluetooth: 4 }]);
    snapshot.counters.wifi_refused = 12;
    snapshot.link_up = false;
    snapshot.link_cause = Some(DisconnectCause::UsbTookBridge);
    let notes = usb(&snapshot).notes;
    assert_eq!(notes, [Note::BufferFull { wifi: 12, bluetooth: 4 }, Note::UsbHoldsBridge]);
    assert!(usb(&snapshot).faults.is_empty(), "a full buffer is no fault");
}

#[test]
fn view_lists_every_fault_most_urgent_first_when_all_are_raised() {
    let mut restarted = node(0x86);
    restarted.state.fault_restart = Some(ResetCause::Brownout);
    restarted.state.fault_restarts = 3;
    let mut snapshot = running(vec![node(0x84), restarted]);
    snapshot.link_up = false;
    snapshot.link_error = Some("unplugged".to_owned());
    snapshot.link_cause = Some(DisconnectCause::NotBonded);
    snapshot.store.dropped = 7;
    snapshot.pool = Some(ChannelPool::Us);
    snapshot.unreachable = ChannelSet::from_run(IndexRun::new(14, 16));
    let mut gps = receiver(GpsStatus::Failed("gone".to_owned()), &snapshot);
    gps.counters.rejected = 300;
    snapshot.gps = Some(gps);
    (snapshot.counters.undecodable, snapshot.counters.garbled) = (1, 5);
    snapshot.counters.incompatible = 2;
    let status = snapshot.bridge_status.as_mut().expect("a status");
    status.dropped_since_attach = 9;
    status.fan = Some(FanStats { duty: FanDuty::new(40).expect("in range"), rpm: Some(0) });
    snapshot.bridge.as_mut().expect("a bridge").reset_cause = ResetCause::Watchdog;

    let faults = usb(&snapshot).faults;
    let link = LinkFault { kind: LinkFaultKind::NotBonded, detail: "unplugged".to_owned() };
    let port = RejectsFrom::Port { port: "/dev/ttyACM1".to_owned(), baud: 9_600 };
    let node = "N9".to_owned();
    assert_eq!(
        faults,
        [
            Fault::LinkDown(link),
            Fault::StoreDropped { rows: 7 },
            Fault::Unswept { pool: ChannelPool::Us, channels: vec![36, 40, 44] },
            Fault::GpsUnreadable { reason: "gone".to_owned() },
            Fault::GpsRejects { lines: 300, from: port },
            Fault::Undecodable { frames: 1 },
            Fault::Garbled { frames: 5 },
            Fault::Incompatible { frames: 2 },
            Fault::BridgeDropped { frames: 9 },
            Fault::BridgeRestarted { cause: ResetCause::Watchdog },
            Fault::NodeRestarted { node, cause: ResetCause::Brownout, times: 3 },
        ],
        "the fan's reading is stale with the link down"
    );
    snapshot.link_up = true;
    let faults = usb(&snapshot).faults;
    assert!(!matches!(faults[0], Fault::LinkDown(_)), "{faults:?}");
    let fan = faults.iter().position(|f| *f == Fault::FanStopped { duty_pct: 40 });
    assert_eq!(fan, Some(faults.len() - 2), "after the bridge's, before the nodes': {faults:?}");
}

#[test]
fn view_lists_no_unswept_fault_when_every_channel_is_dealt_or_no_pool_is_reported() {
    let mut snapshot = running(Vec::new());
    snapshot.pool = Some(ChannelPool::Us);
    assert!(usb(&snapshot).faults.is_empty());
    snapshot.pool = None;
    snapshot.unreachable = ChannelPool::All.channels();
    assert!(usb(&snapshot).faults.is_empty());
}

#[test]
fn view_lists_fan_fault_only_when_fan_is_still_at_a_turning_duty_with_link_up() {
    let faults = |snapshot: &Snapshot| usb(snapshot).faults;
    assert_eq!(faults(&with_fan(40, Some(0))), [Fault::FanStopped { duty_pct: 40 }]);
    assert_eq!(faults(&with_fan(20, Some(0))), [Fault::FanStopped { duty_pct: 20 }]);
    // Turning; not read since the bridge booted; and duties the fan may stop at.
    for (pct, rpm) in [(40, Some(1830)), (40, None), (0, Some(0)), (10, Some(0)), (19, Some(0))] {
        assert!(faults(&with_fan(pct, rpm)).is_empty(), "{pct}% {rpm:?}");
    }
    let mut down = with_fan(40, Some(0));
    down.link_up = false;
    assert!(faults(&down).is_empty(), "a stale reading");
}

#[test]
fn view_lists_bridge_restart_only_when_cause_is_not_power_on() {
    let restarted = |cause| {
        let mut snapshot = running(Vec::new());
        snapshot.bridge.as_mut().expect("a bridge").reset_cause = cause;
        usb(&snapshot).faults
    };
    assert!(restarted(ResetCause::PowerOn).is_empty(), "how a capture starts");
    for cause in [
        ResetCause::Software,
        ResetCause::Watchdog,
        ResetCause::Lockup,
        ResetCause::Brownout,
        ResetCause::External,
        ResetCause::Unknown,
    ] {
        assert_eq!(restarted(cause), [Fault::BridgeRestarted { cause }]);
    }
}

#[test]
fn view_lists_bridge_drops_only_when_this_capture_lost_frames() {
    let mut snapshot = running(Vec::new());
    snapshot.bridge_status.as_mut().expect("a status").dropped_tx = 1_300;
    assert!(usb(&snapshot).faults.is_empty(), "frames dropped before this host attached");
    snapshot.bridge_status.as_mut().expect("a status").dropped_since_attach = 5;
    assert_eq!(usb(&snapshot).faults, [Fault::BridgeDropped { frames: 5 }]);
}

#[test]
fn view_names_restarted_node_by_address_tail_when_node_has_no_slot() {
    let mut slotless = node(0x86);
    slotless.state.slot = None;
    slotless.state.fault_restart = Some(ResetCause::Brownout);
    slotless.state.fault_restarts = 1;
    let faults = usb(&running(vec![slotless])).faults;
    let restart =
        Fault::NodeRestarted { node: "57:86".to_owned(), cause: ResetCause::Brownout, times: 1 };
    assert_eq!(faults, [restart]);
}

/// The rejected-lines fault for a receiver with `rejected` lines and `fixes`, set up by `setup`.
fn rejects(rejected: u64, fixes: u64, setup: fn(&mut GpsView)) -> Vec<Fault> {
    let mut snapshot = with_gps(GpsStatus::Searching, PositionSource::None);
    let gps = snapshot.gps.as_mut().expect("a receiver");
    gps.counters = GpsCounters { sentences: 0, fixes, rejected };
    setup(gps);
    usb(&snapshot).faults
}

#[test]
fn view_lists_gps_rejects_by_where_lines_come_from_when_no_fix_has_arrived() {
    let fault = |from| vec![Fault::GpsRejects { lines: 300, from }];
    assert_eq!(rejects(300, 0, |gps| gps.via_bridge = true), fault(RejectsFrom::Bridge));
    assert_eq!(rejects(300, 0, |gps| gps.pinned_baud = true), fault(RejectsFrom::PinnedBaud));
    let port = RejectsFrom::Port { port: "/dev/ttyACM1".to_owned(), baud: 9_600 };
    assert_eq!(rejects(300, 0, |_| {}), fault(port));
    assert_eq!(rejects(300, 0, |gps| gps.settled = None), fault(RejectsFrom::Unknown));
}

#[test]
fn view_lists_no_gps_rejects_when_lines_are_few_or_a_fix_arrived() {
    assert!(rejects(20, 0, |_| {}).is_empty(), "joining a stream mid-sentence");
    assert!(rejects(300, 1, |_| {}).is_empty());
    let none = usb(&with_gps(GpsStatus::NoReceiver, PositionSource::Static));
    assert!(none.faults.is_empty(), "a missing receiver is no fault");
}

/// A running capture whose bridge reports `sd` in its slot, a weighted planner and the scan.
fn with_settings(sd: Option<SdStatus>) -> Snapshot {
    let mut snapshot = with_fan(40, Some(1830));
    snapshot.pool = Some(ChannelPool::Eu);
    let sparse_nodes = SparseNodes::fixed(2).expect("in range");
    snapshot.planner = Some(PlanSettings { planner: Planner::Weighted, sparse_nodes });
    snapshot.wardrive_esp = Some(true);
    snapshot.sd_log = sd.map(|sd| sd.log.on);
    snapshot.scan = Some(ScanMode::WifiBluetooth);
    snapshot.gps_rate_hz = 2;
    snapshot.excluded_ssids = vec!["HomeNet".to_owned()];
    if let Some(status) = &mut snapshot.bridge_status {
        status.sd = sd;
    }
    snapshot
}

#[test]
fn view_shows_bridge_settings_when_link_is_up() {
    let card = SdStatus {
        log: SdLog { on: true, ..SdLog::OFF },
        ..SdStatus::card(SdState::Ready, 60_350, Some(55_000))
    };
    let shown = usb(&with_settings(Some(card))).settings;
    let log = SdLogShown { on: true, ..SdLogShown::from(SdLog::OFF) };
    let sd = SdShown::Card {
        state: SdState::Ready,
        card_mb: 60_350,
        free_mb: Some(55_000),
        log,
        can_format: true,
    };
    let want = SettingsShown {
        pool: Some(ChannelPool::Eu),
        planner: Some(PlannerShown { planner: Planner::Weighted, sparse_nodes: Some(2) }),
        fan_duty_pct: Some(40),
        wardrive_esp: Some(true),
        sd_log: Some(true),
        bluetooth: Some(true),
        gps_rate_hz: 2,
        excluded_ssids: vec!["HomeNet".to_owned()],
        sd,
    };
    assert_eq!(shown, want);
}

#[test]
fn view_shows_no_bridge_settings_when_link_is_down() {
    let mut down = with_settings(Some(SdStatus::card(SdState::Ready, 60_350, None)));
    down.link_up = false;
    let shown = usb(&down).settings;
    assert_eq!(
        (shown.pool, shown.planner, shown.fan_duty_pct, shown.wardrive_esp, shown.sd_log),
        (None, None, None, None, None)
    );
    assert_eq!(shown.sd, SdShown::NoBridge);
    // The run holds the scan, and these the host, so they stay.
    assert_eq!(shown.bluetooth, Some(true));
    assert_eq!(shown.gps_rate_hz, 2);
    assert_eq!(shown.excluded_ssids, ["HomeNet"]);
}

#[test]
fn view_shows_nothing_to_toggle_when_scan_is_stopped() {
    let stopped = Snapshot { scan: Some(ScanMode::Stopped), ..with_settings(None) };
    assert_eq!(usb(&stopped).settings.bluetooth, None);
    let unreported = Snapshot { scan: None, ..with_settings(None) };
    assert_eq!(usb(&unreported).settings.bluetooth, Some(false), "off before the first report");
}

#[test]
fn view_reads_planner_count_as_automatic_when_sparse_nodes_is_auto() {
    let settings = PlanSettings { planner: Planner::Even, sparse_nodes: SparseNodes::AUTO };
    let shown = PlannerShown::from(settings);
    assert_eq!(shown, PlannerShown { planner: Planner::Even, sparse_nodes: None });
    assert_eq!(PlanSettings::from(shown), settings);
}

#[test]
fn view_offers_format_only_when_card_is_not_fat32_ready_or_failed() {
    let can_format =
        |state| match usb(&with_settings(Some(SdStatus::card(state, 30_436, None)))).settings.sd {
            SdShown::Card { can_format, .. } => can_format,
            other => panic!("{state:?}: {other:?}"),
        };
    assert!(can_format(SdState::NoFat32));
    assert!(can_format(SdState::Ready));
    assert!(can_format(SdState::Failed));
    assert!(!can_format(SdState::NoCard));
    assert!(!can_format(SdState::Formatting));
}

#[test]
fn view_shows_sd_waiting_or_no_slot_when_bridge_has_not_reported_one() {
    assert_eq!(usb(&with_settings(None)).settings.sd, SdShown::NoSlot);
    let waiting = Snapshot { bridge_status: None, ..with_settings(None) };
    assert_eq!(usb(&waiting).settings.sd, SdShown::Waiting);
}

#[test]
fn view_carries_bridge_answers_when_bridge_has_answered() {
    let mut snapshot = running(Vec::new());
    assert_eq!(usb(&snapshot).answers, Answers { power: None, sd: None });
    let slot = Slot::new(1, 0).expect("B1");
    let board = NodeBoard::Carrier14;
    snapshot.power_answer = Some(PowerAnswer { id: 3, slot, board, outcome: PowerOutcome::Failed });
    snapshot.sd_answer = Some(SdAnswer { id: 1, outcome: SdFormatOutcome::Busy });
    let answers = usb(&snapshot).answers;
    let slot = SlotShown { index: slot.as_u8(), label: "N8".to_owned() };
    assert_eq!(answers.power, Some(PowerAnswered { id: 3, slot, outcome: PowerOutcome::Failed }));
    assert_eq!(answers.sd, Some(SdAnswered { id: 1, outcome: SdFormatOutcome::Busy }));
}

/// A bridge on `version` reached and read for its core alone, as the engine keeps it.
fn reached_unread(version: Version) -> Snapshot {
    let mut snapshot = running(Vec::new());
    snapshot.bridge = None;
    snapshot.link_up = false;
    snapshot.store.dropped = 7;
    snapshot.incompatible = Some(BridgeCore {
        mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
        image: None,
        version,
        trial: Trial::Settled,
        uptime_ms: 400,
        standing: Standing::of(version, BRIDGE_VERSION, false),
    });
    snapshot
}

#[test]
fn view_lists_incompatible_bridge_first_when_bridge_is_reached_but_unread() {
    let older = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
    let faults = usb(&reached_unread(older)).faults;
    let standing = Standing::Apart { older: End::Host };
    let first = Fault::BridgeIncompatible { bridge: older, host: BRIDGE_VERSION, standing };
    assert_eq!(faults, [first, Fault::StoreDropped { rows: 7 }], "in place of a link fault");

    let faults = usb(&reached_unread(BRIDGE_VERSION)).faults;
    let standing = Standing::OtherBuild;
    let first =
        Fault::BridgeIncompatible { bridge: BRIDGE_VERSION, host: BRIDGE_VERSION, standing };
    assert_eq!(faults[0], first);
}

#[test]
fn view_notes_older_end_when_bridge_is_a_minor_apart() {
    let newer = Version::new(BRIDGE_VERSION.major, BRIDGE_VERSION.minor + 1, 0);
    let mut snapshot = running(Vec::new());
    if let Some(bridge) = &mut snapshot.bridge {
        bridge.version = newer;
    }
    let view = usb(&snapshot);
    let note = Note::BridgeMinorApart { bridge: newer, host: BRIDGE_VERSION, older: End::Host };
    assert_eq!(view.notes, [note]);
    assert!(view.faults.is_empty(), "the capture runs: {:?}", view.faults);

    // The same minor says nothing.
    assert!(usb(&running(Vec::new())).notes.is_empty());
}
