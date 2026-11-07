//! The simulated fleet.
//!
//! Runs on tokio's paused clock, so a sweep that takes seconds of wall time
//! completes instantly and deterministically.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::bus::{
    Assign, Clear, DecodeError, DownType, Heartbeat, NodeBoard, RecordKind, Slot, UpMsg, Version,
};
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::link::{BridgeToHost, HostToBridge, PowerOutcome, ToNodeBody, Undelivered};
use wartui_proto::node::{ASSIGNED_BEAT_MS, CHANNEL_DWELL_MS, IDLE_BEAT_MS};
use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun};

/// One record a node sent, as the bridge forwarded it.
///
/// Held as owned bytes because a `SightingMsg` borrows its SSID from the body, so the caller
/// has to own the bytes to hold one.
struct Rec {
    slot: Slot,
    kind: u8,
    body: Vec<u8>,
}

impl Rec {
    fn msg(&self) -> Result<UpMsg<'_>, DecodeError> {
        UpMsg::decode(self.kind, &self.body)
    }
}

/// The next record a node sent.
async fn next_frame(link: &mut LinkHandle) -> Rec {
    loop {
        if let LinkEvent::Message(BridgeToHost::FromNode { slot, kind, body, .. }) =
            link.recv().await.expect("simulator keeps running")
        {
            let rec = Rec { slot, kind, body: body.to_vec() };
            rec.msg().expect("simulator emits valid records");
            return rec;
        }
    }
}

fn heartbeat_of(rec: &Rec) -> Option<Heartbeat> {
    match rec.msg() {
        Ok(UpMsg::Heartbeat(heartbeat)) => Some(heartbeat),
        _ => None,
    }
}

/// Wait for the next heartbeat from `want`, returning when it arrived.
async fn next_heartbeat_from(link: &mut LinkHandle, want: Slot) -> Instant {
    loop {
        let rec = next_frame(link).await;
        if rec.slot == want && heartbeat_of(&rec).is_some() {
            return Instant::now();
        }
    }
}

/// Wait for the next heartbeat from `want`, returning when it arrived and the
/// counter it carried — the sweep-count proof, since a heartbeat's own cadence
/// is fixed and carries none.
async fn next_heartbeat_counter_from(link: &mut LinkHandle, want: Slot) -> (Instant, u32) {
    loop {
        let rec = next_frame(link).await;
        if rec.slot == want
            && let Some(heartbeat) = heartbeat_of(&rec)
        {
            return (Instant::now(), heartbeat.counter);
        }
    }
}

/// Wait for the next heartbeat from `want`, returning it decoded.
async fn next_heartbeat_msg_from(link: &mut LinkHandle, want: Slot) -> Heartbeat {
    loop {
        let rec = next_frame(link).await;
        if rec.slot == want
            && let Some(heartbeat) = heartbeat_of(&rec)
        {
            return heartbeat;
        }
    }
}

fn assign_command(slot: Slot, assign: Assign) -> HostToBridge {
    let body = ToNodeBody::from_slice(&assign.encode()).expect("an assignment fits");
    HostToBridge::ToNode { slot, id: 1, kind: DownType::Assign.as_u8(), body }
}

fn clear_command(slot: Slot) -> HostToBridge {
    let body = ToNodeBody::from_slice(&Clear.encode()).expect("a clear fits");
    HostToBridge::ToNode { slot, id: 1, kind: DownType::Clear.as_u8(), body }
}

/// A slot the default fleets leave empty.
fn empty_slot() -> Slot {
    Slot::new(1, 6).expect("B7")
}

/// An assignment for node 0, since almost every test needs one.
///
/// A wartui node idles and collects nothing until told what to scan, so a test that
/// wants to see anything has to do first what the host does on the first heartbeat.
///
/// An empty `channels` with `ble` set is the Bluetooth-only assignment: that node
/// sniffs nothing, and the flag is what says why. Empty with `ble` clear is the one
/// thing the host never sends, which the node treats as never having been told
/// anything.
fn assign(link: &LinkHandle, version: u8, channels: ChannelSet, ble: bool) {
    link.send_urgent(assign_command(
        SimTransport::node_slot(0),
        Assign { epoch: version, flags: Assign::flags_for(ble), channels },
    ))
    .expect("queued");
}

/// How many distinct Wi-Fi addresses are among the first `sightings` a lone node reports
/// from a neighborhood of forty networks.
async fn distinct_addresses(sightings_per_address: Option<u32>, sightings: usize) -> usize {
    let mut link = SimTransport::new(SimConfig {
        node_count: 1,
        wifi_networks: 40,
        ble_chance: 0.0,
        sightings_per_address,
        ..SimConfig::default()
    })
    .start()
    .expect("simulator starts");
    assign(&link, 1, ChannelPool::All.channels(), false);

    let mut seen = HashSet::new();
    let mut reported = 0;
    'outer: while reported < sightings {
        let rec = next_frame(&mut link).await;
        let Ok(UpMsg::Sightings(batch)) = rec.msg() else { continue };
        for (sighting, _) in batch.iter() {
            seen.insert(sighting.bssid);
            reported += 1;
            if reported >= sightings {
                break 'outer;
            }
        }
    }
    seen.len()
}

#[tokio::test(start_paused = true)]
async fn sim_transport_tracks_distinct_macs_when_neighborhood_is_moving_vs_parked() {
    // Parked, forty networks fit the dedup ring, so whatever is reported again is one of
    // the same forty: the ring's refresh, not a new device.
    assert!(distinct_addresses(None, 120).await <= 40);
    // Moving on after two hearings, a network the ring suppressed once comes back under
    // an address it has never held, so every report is a device not seen before.
    assert_eq!(distinct_addresses(Some(2), 120).await, 120);
}

/// The next answer the bridge gives a `ToNode`.
async fn next_send_result(link: &mut LinkHandle) -> Result<u16, Undelivered> {
    loop {
        match link.recv().await.expect("running") {
            LinkEvent::Message(BridgeToHost::Delivered { id }) => return Ok(id),
            LinkEvent::Message(BridgeToHost::Undelivered { reason, .. }) => return Err(reason),
            _ => {}
        }
    }
}

/// Every channel there is, which is what a lone node on the `All` pool holds.
fn everything() -> ChannelSet {
    ChannelPool::All.channels()
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_emits_connected_event_first_when_simulation_starts() {
    let mut link = SimTransport::new(SimConfig::default()).start().expect("starts");
    match link.recv().await.expect("an event") {
        LinkEvent::Connected(info) => {
            assert!(info.fw_version.ends_with("-sim"));
        }
        other => panic!("expected Connected first, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_receives_heartbeats_from_all_nodes_when_fleet_runs() {
    let config = SimConfig { node_count: 4, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    let mut seen: HashSet<(Slot, [u8; 6])> = HashSet::new();
    while seen.len() < 4 {
        let rec = next_frame(&mut link).await;
        if let Some(heartbeat) = heartbeat_of(&rec) {
            seen.insert((rec.slot, heartbeat.mac));
        }
    }
    // Node i sits in slot i, and its heartbeat names its own MAC.
    let expected: HashSet<(Slot, [u8; 6])> =
        (0..4).map(|i| (SimTransport::node_slot(i), SimTransport::node_mac(i))).collect();
    assert_eq!(seen, expected);
}

#[tokio::test(start_paused = true)]
async fn sim_node_increments_heartbeat_counter_monotonically_when_operating() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    let mut counters = Vec::new();
    while counters.len() < 3 {
        if let Some(heartbeat) = heartbeat_of(&next_frame(&mut link).await) {
            counters.push(heartbeat.counter);
        }
    }
    assert_eq!(counters, vec![1, 2, 3], "a fresh node counts up from its boot");
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_emits_valid_sightings_with_negative_rssi_when_nodes_sniff() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, everything(), true);

    let mut checked = 0;
    while checked < 20 {
        let rec = next_frame(&mut link).await;
        let Ok(UpMsg::Sightings(batch)) = rec.msg() else { continue };
        for (sighting, _) in batch.iter() {
            assert!(sighting.rssi < 0, "signal strengths are negative dBm");
            checked += 1;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn sim_node_deduplicates_wifi_sightings_when_operating_in_static_neighborhood() {
    // A simulator that streamed the same networks forever would leave the fleet view
    // wrong about observation rates.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, everything(), false);

    let mut counts: HashMap<[u8; 6], usize> = HashMap::new();
    let mut sweeps = 0;
    while sweeps < 3 {
        let rec = next_frame(&mut link).await;
        match rec.msg() {
            Ok(UpMsg::Heartbeat(_)) => sweeps += 1,
            Ok(UpMsg::Sightings(batch)) => {
                for (sighting, _) in batch.iter() {
                    *counts.entry(sighting.bssid).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    assert!(!counts.is_empty(), "the fake neighborhood should not be empty");
    let repeats: Vec<_> = counts.iter().filter(|(_, n)| **n > 1).collect();
    assert!(repeats.is_empty(), "networks reported more than once over 3 sweeps: {repeats:?}");
}

#[tokio::test(start_paused = true)]
async fn sim_node_generates_new_ble_sightings_when_advertiser_addresses_rotate() {
    let config = SimConfig { node_count: 1, ble_chance: 1.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, ChannelSet::empty(), true);

    let mut ble = HashSet::new();
    let mut sweeps = 0;
    while sweeps < 3 {
        let rec = next_frame(&mut link).await;
        match rec.msg() {
            Ok(UpMsg::Heartbeat(_)) => sweeps += 1,
            Ok(UpMsg::Sightings(batch)) => {
                for (sighting, _) in batch.iter().filter(|(s, _)| s.kind == RecordKind::Ble) {
                    ble.insert(sighting.bssid);
                }
            }
            _ => {}
        }
    }
    assert!(ble.len() > 10, "rotating BLE addresses should keep producing new sightings");
}

#[tokio::test(start_paused = true)]
async fn sim_node_heartbeats_at_assigned_beat_when_holding_bluetooth_scan() {
    // The heartbeat cadence is ASSIGNED_BEAT_MS, checked only between scans. The
    // schedule advances from the deadline rather than the fire time, so every gap stays
    // within one scan step of ASSIGNED_BEAT_MS, and the mean is exact.
    const SIM_SCAN_MS: u128 = 500; // mirrors the sim's own scan length

    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, ChannelSet::empty(), true);

    let mut beats = Vec::new();
    let mut ble = 0;
    while beats.len() < 12 {
        let rec = next_frame(&mut link).await;
        match rec.msg() {
            Ok(UpMsg::Heartbeat(_)) => beats.push(tokio::time::Instant::now()),
            Ok(UpMsg::Sightings(batch)) => {
                ble += batch.iter().filter(|(s, _)| s.kind == RecordKind::Ble).count();
            }
            _ => {}
        }
    }
    let gaps: Vec<u128> = beats.windows(2).map(|pair| (pair[1] - pair[0]).as_millis()).collect();
    // The first gap straddles the idle-to-assigned transition and is off the
    // schedule; every gap after it is on it, across a full wrap of the cycle.
    let checked = &gaps[1..];
    assert!(
        checked.iter().all(|gap| gap.abs_diff(u128::from(ASSIGNED_BEAT_MS)) <= SIM_SCAN_MS),
        "every gap within one scan step of ASSIGNED_BEAT_MS: {gaps:?}"
    );
    let mean = checked.iter().sum::<u128>() / checked.len() as u128;
    assert_eq!(
        mean,
        u128::from(ASSIGNED_BEAT_MS),
        "no cumulative drift over a full wrap: {gaps:?}"
    );
    assert!(ble > 0, "and the default advertiser rate still produces some: {ble}");
}

#[tokio::test(start_paused = true)]
async fn sim_node_reports_only_ble_sightings_when_holding_ble_flag() {
    // The point of the split: a fleet of one holding the scan sweeps nothing, so
    // every sighting it produces is an advertiser, reported every scan rather than
    // every sweep.
    let config = SimConfig { node_count: 1, ble_chance: 1.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, ChannelSet::empty(), true);

    let mut wifi = 0;
    let mut ble = 0;
    let mut beats = 0;
    while beats < 4 {
        let rec = next_frame(&mut link).await;
        match rec.msg() {
            Ok(UpMsg::Heartbeat(_)) => beats += 1,
            Ok(UpMsg::Sightings(batch)) => {
                for (sighting, _) in batch.iter() {
                    if sighting.kind == RecordKind::Ble { ble += 1 } else { wifi += 1 }
                }
            }
            _ => {}
        }
    }
    assert_eq!(wifi, 0, "a Bluetooth node sniffs no Wi-Fi at all");
    assert!(ble > 10, "and reports advertisers every cycle instead: {ble}");
}

#[tokio::test(start_paused = true)]
async fn sim_node_idles_without_sightings_when_assigned_empty_channels_without_ble() {
    // The one assignment the host never sends. A node that took it literally would loop
    // on nothing, so it counts as never having been told anything: it idles, which
    // means the idle beat and no sightings of any kind.
    let config = SimConfig { node_count: 1, ble_chance: 1.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, ChannelSet::empty(), false);

    let mut sightings = 0;
    let mut beats = 0;
    while beats < 4 {
        let rec = next_frame(&mut link).await;
        match rec.msg() {
            Ok(UpMsg::Heartbeat(_)) => beats += 1,
            Ok(UpMsg::Sightings(batch)) => sightings += batch.iter().count(),
            _ => {}
        }
    }
    assert_eq!(sightings, 0, "an idle node collects nothing");
}

#[tokio::test(start_paused = true)]
async fn sim_node_reports_epoch_zero_until_assigned_then_the_adopted_epoch() {
    // The heartbeat carries the epoch the node holds, so adoption is confirmed
    // by the epoch on the next heartbeat rather than inferred from cadence.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);

    let parked = next_heartbeat_msg_from(&mut link, node).await;
    assert_eq!(parked.epoch, 0, "unassigned since boot, nothing adopted yet");

    assign(&link, 1, everything(), false);

    // Skip the heartbeat that straddles the assignment landing mid-sweep.
    next_heartbeat_msg_from(&mut link, node).await;
    let adopted = next_heartbeat_msg_from(&mut link, node).await;
    assert_eq!(adopted.epoch, 1, "the next clean heartbeat reports the adopted epoch");
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_announces_fourteen_node_carrier_when_simulation_starts() {
    let mut link = SimTransport::new(SimConfig::default()).start().expect("starts");
    match link.recv().await.expect("an event") {
        LinkEvent::Connected(info) => assert_eq!(info.node_board, NodeBoard::Carrier14),
        other => panic!("expected Connected first, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn sim_node_boots_afresh_when_power_cycled() {
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);
    assign(&link, 1, everything(), false);
    while next_heartbeat_msg_from(&mut link, node).await.epoch != 1 {}

    link.send_bulk(HostToBridge::PowerCycle { slot: node }).expect("queued");
    loop {
        if let LinkEvent::Message(BridgeToHost::PowerCycled { slot, outcome }) =
            link.recv().await.expect("simulator keeps running")
        {
            assert_eq!((slot, outcome), (node, PowerOutcome::Done));
            break;
        }
    }
    // A heartbeat already on its way may arrive first; the reboot's follows it.
    let mut rebooted = false;
    for _ in 0..3 {
        let beat = next_heartbeat_msg_from(&mut link, node).await;
        if beat.epoch == 0 {
            rebooted = true;
            break;
        }
    }
    assert!(rebooted, "the node forgot its assignment, as a real reboot does");
}

#[tokio::test(start_paused = true)]
async fn sim_node_reports_new_epoch_when_channel_range_is_narrowed() {
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);
    assign(&link, 1, everything(), false);

    next_heartbeat_msg_from(&mut link, node).await;
    let first = next_heartbeat_msg_from(&mut link, node).await;
    assert_eq!(first.epoch, 1);

    let mut one = ChannelSet::empty();
    one.insert(0);
    assign(&link, 2, one, false);

    // Skip the heartbeat that straddles the change, then check a clean one.
    next_heartbeat_msg_from(&mut link, node).await;
    let narrowed = next_heartbeat_msg_from(&mut link, node).await;
    assert_eq!(narrowed.epoch, 2, "the heartbeat reports the new epoch after narrowing");
}

#[tokio::test(start_paused = true)]
async fn sim_node_heartbeats_at_assigned_beat_when_sweeping() {
    // A one-channel share sweeps many times inside one heartbeat interval: the
    // beat is checked only between dwells, and the schedule advances from the
    // deadline rather than the fire time, so every gap stays within one dwell of
    // ASSIGNED_BEAT_MS and the mean is exact, while the counter climbs by many
    // sweeps between any two beats.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);
    let mut one = ChannelSet::empty();
    one.insert(0);
    assign(&link, 1, one, false);

    let mut beats = Vec::new();
    while beats.len() < 12 {
        beats.push(next_heartbeat_counter_from(&mut link, node).await);
    }
    let gaps: Vec<u128> =
        beats.windows(2).map(|pair| (pair[1].0 - pair[0].0).as_millis()).collect();
    // The first gap straddles the idle-to-assigned transition and is off the
    // schedule; every gap after it is on it, across a full wrap of the cycle.
    let checked = &gaps[1..];
    assert!(
        checked
            .iter()
            .all(|gap| gap.abs_diff(u128::from(ASSIGNED_BEAT_MS)) <= u128::from(CHANNEL_DWELL_MS)),
        "every gap within one dwell step of ASSIGNED_BEAT_MS: {gaps:?}"
    );
    let mean = checked.iter().sum::<u128>() / checked.len() as u128;
    assert_eq!(
        mean,
        u128::from(ASSIGNED_BEAT_MS),
        "no cumulative drift over a full wrap: {gaps:?}"
    );

    let (_, c1) = beats[1];
    let (_, c2) = beats[2];
    assert!(c2 - c1 > 1, "many sweeps complete inside one heartbeat interval: {c1} -> {c2}");
}

#[tokio::test(start_paused = true)]
async fn sim_node_waits_a_full_beat_when_reassigned_after_idling() {
    // A stale `next_beat` left over from a share this node held before it
    // went idle must not fire on the very first dwell once it is reassigned.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);
    let mut one = ChannelSet::empty();
    one.insert(0);

    assign(&link, 1, one, false);
    // Let its next_beat deadline pass at least once while assigned, so it is
    // well in the past by the time this node goes idle.
    next_heartbeat_from(&mut link, node).await;
    next_heartbeat_from(&mut link, node).await;

    // An assignment with no channels and no Bluetooth flag is the one thing
    // the host never sends, and this node treats it as never having been told
    // anything: it idles.
    assign(&link, 2, ChannelSet::empty(), false);

    // Drain idle heartbeats until a whole beat interval has passed while
    // idle, so a stale deadline would be long overdue by the time this node
    // is reassigned. Draining rather than sleeping blind is what keeps a
    // backlog of idle beats from being mistaken for the first fresh one below.
    let past_a_beat =
        tokio::time::Instant::now() + Duration::from_millis(u64::from(ASSIGNED_BEAT_MS));
    let mut last_idle_at = next_heartbeat_from(&mut link, node).await;
    while tokio::time::Instant::now() < past_a_beat {
        last_idle_at = next_heartbeat_from(&mut link, node).await;
    }

    assign(&link, 3, one, false);
    let first = next_heartbeat_from(&mut link, node).await;
    let gap = (first - last_idle_at).as_millis();
    // At least a whole interval — the fix — never a mere dwell,
    // which is what the stale-deadline bug produced. The idle beat already in
    // flight when the reassignment lands can add up to one more IDLE_BEAT_MS,
    // since it is not cut short.
    assert!(
        gap >= u128::from(ASSIGNED_BEAT_MS) && gap <= u128::from(ASSIGNED_BEAT_MS + IDLE_BEAT_MS),
        "the first heartbeat after reassignment waits a full interval, not one dwell: {gap} ms"
    );
}

#[tokio::test(start_paused = true)]
async fn sim_node_advances_counter_once_per_scan_when_holding_bluetooth_scan() {
    // The counter is the proof visible without serial access, and it should climb
    // at a steady rate every heartbeat interval: one count per scan, not one per
    // heartbeat.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);
    assign(&link, 1, ChannelSet::empty(), true);

    next_heartbeat_from(&mut link, node).await;
    let (_, c1) = next_heartbeat_counter_from(&mut link, node).await;
    let (_, c2) = next_heartbeat_counter_from(&mut link, node).await;
    let (_, c3) = next_heartbeat_counter_from(&mut link, node).await;

    assert!(c2 - c1 > 1, "several scans complete inside one heartbeat interval: {c1} -> {c2}");
    assert_eq!(c2 - c1, c3 - c2, "a steady per-scan rate: {c1}, {c2}, {c3}");
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_reports_delivered_when_assignment_sent_to_active_node() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, ChannelSet::from_run(IndexRun::new(4, 8)), false);
    assert_eq!(next_send_result(&mut link).await, Ok(1));
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_reports_no_node_when_assignment_sent_to_empty_slot() {
    // The host must be able to tell delivery from a successful enqueue.
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    link.send_urgent(assign_command(
        empty_slot(),
        Assign { epoch: 1, flags: 0, channels: ChannelSet::from_run(IndexRun::new(0, 3)) },
    ))
    .expect("queued");
    assert_eq!(next_send_result(&mut link).await, Err(Undelivered::NoNode));
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_reports_delivered_when_clear_sent_to_active_node() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    link.send_urgent(clear_command(SimTransport::node_slot(0))).expect("queued");
    assert_eq!(next_send_result(&mut link).await, Ok(1));
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_reports_no_node_when_clear_sent_to_empty_slot() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    link.send_urgent(clear_command(empty_slot())).expect("queued");
    assert_eq!(next_send_result(&mut link).await, Err(Undelivered::NoNode));
}

#[tokio::test(start_paused = true)]
async fn sim_node_ignores_retransmitted_assignment_when_epoch_version_is_unchanged() {
    // Nodes compare with `!=`, and a node that has never heard the host holds
    // version 0. Re-sending the version it already has changes nothing, which
    // is exactly why the host must persist a monotonic counter across restarts.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_slot(0);

    assign(&link, 7, everything(), false);
    // Skip the sweep that straddles the change.
    next_heartbeat_from(&mut link, node).await;

    // The same epoch, saying something completely different: acknowledged and then
    // discarded.
    let mut one = ChannelSet::empty();
    one.insert(0);
    assign(&link, 7, one, false);

    let first = next_heartbeat_from(&mut link, node).await;
    let second = next_heartbeat_from(&mut link, node).await;
    let period = second - first;
    assert!(
        period > tokio::time::Duration::from_millis(2000),
        "epoch 7 is what the node already holds, so it should still be sweeping \
         the whole pool, but the period was {period:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_answers_status_request_when_queried() {
    let mut link = SimTransport::new(SimConfig::default()).start().expect("starts");
    link.send_bulk(HostToBridge::GetStatus).expect("queued");

    loop {
        if let LinkEvent::Message(BridgeToHost::Status { .. }) = link.recv().await.expect("running")
        {
            return;
        }
    }
}

/// The fan the next `Status` reports.
async fn next_fan(link: &mut LinkHandle) -> Option<FanStats> {
    link.send_bulk(HostToBridge::GetStatus).expect("queued");
    loop {
        if let LinkEvent::Message(BridgeToHost::Status { fan, .. }) =
            link.recv().await.expect("running")
        {
            return fan;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn sim_bridge_reports_duty_set_when_host_sets_fan() {
    let mut link = SimTransport::new(SimConfig::default()).start().expect("starts");
    let full = next_fan(&mut link).await.expect("a carrier has a fan");
    assert_eq!(full.duty, FanDuty::FULL, "no flash, so it starts at full");

    let duty = FanDuty::new(40).expect("in range");
    link.send_bulk(HostToBridge::SetFan { duty }).expect("queued");
    let set = next_fan(&mut link).await.expect("a carrier has a fan");

    assert_eq!(set.duty, duty);
    assert!(set.rpm.expect("a speed") < full.rpm.expect("a speed"), "slower at 40%");
}

#[tokio::test(start_paused = true)]
async fn sim_transport_produces_deterministic_sightings_when_seeded() {
    async fn first_sightings(seed: u64) -> Vec<Vec<u8>> {
        let config = SimConfig { node_count: 1, seed, ..SimConfig::default() };
        let mut link = SimTransport::new(config).start().expect("starts");
        assign(&link, 1, everything(), true);
        let mut frames = Vec::new();
        while frames.len() < 10 {
            let rec = next_frame(&mut link).await;
            if matches!(rec.msg(), Ok(UpMsg::Sightings(_))) {
                frames.push(rec.body);
            }
        }
        frames
    }

    assert_eq!(first_sightings(1234).await, first_sightings(1234).await);
    assert_ne!(first_sightings(1234).await, first_sightings(9999).await);
}

#[tokio::test(start_paused = true)]
async fn sim_fleet_restricts_ble_reports_to_assigned_node_when_partitioned() {
    let config = SimConfig { node_count: 2, ble_chance: 1.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    // The shape the host sends: the scanner gets the flag and no channels, and the
    // whole pool goes to the node still sniffing.
    assign(&link, 1, ChannelSet::empty(), true);
    link.send_urgent(assign_command(
        SimTransport::node_slot(1),
        Assign { epoch: 1, flags: 0, channels: everything() },
    ))
    .expect("queued");

    // A room where an advertiser turns up in every slot of every scan, so a node
    // that was going to report one has had every chance to.
    let mut ble_by_node: HashMap<Slot, usize> = HashMap::new();
    let mut sweeps = 0;
    while sweeps < 3 {
        let rec = next_frame(&mut link).await;
        match rec.msg() {
            Ok(UpMsg::Heartbeat(_)) if rec.slot == SimTransport::node_slot(1) => sweeps += 1,
            Ok(UpMsg::Sightings(batch)) => {
                let ble = batch.iter().filter(|(s, _)| s.kind == RecordKind::Ble).count();
                if ble > 0 {
                    *ble_by_node.entry(rec.slot).or_default() += ble;
                }
            }
            _ => {}
        }
    }
    assert!(ble_by_node[&SimTransport::node_slot(0)] > 5, "the node that was asked");
    assert_eq!(ble_by_node.get(&SimTransport::node_slot(1)), None, "and only that node");
}

#[test]
fn sim_minor_behind_steps_back_one_minor_when_minor_is_above_zero() {
    use wartui_bridge::sim::minor_behind;
    assert_eq!(minor_behind(Version::new(0, 3, 7)), Version::new(0, 2, 7));
    assert_eq!(minor_behind(Version::new(2, 1, 0)), Version::new(2, 0, 0));
}

#[test]
#[should_panic(expected = "has no minor before it")]
fn sim_minor_behind_panics_when_minor_is_zero() {
    let _ = wartui_bridge::sim::minor_behind(Version::new(1, 0, 4));
}
