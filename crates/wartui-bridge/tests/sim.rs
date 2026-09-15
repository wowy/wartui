//! The simulated fleet.
//!
//! Runs on tokio's paused clock, so a sweep that takes seconds of wall time
//! completes instantly and deterministically.

use std::collections::{HashMap, HashSet};

use tokio::time::Instant;
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::air::{AdminMsg, Frame, HeartbeatMsg, RecordKind, SightingMsg};
use wartui_proto::link::{BridgeToHost, EspNowPayload, HostToBridge, Mac, SendStatus};
use wartui_proto::plan::{ChannelPool, ChannelSet, IndexRun};

/// The next node → core frame, as (source, the bytes it arrived as).
///
/// Handed back raw rather than decoded because a `SightingMsg` borrows its
/// SSID from the frame, so the caller has to own the bytes to hold one.
async fn next_frame(link: &mut LinkHandle) -> (Mac, Vec<u8>) {
    loop {
        match link.recv().await.expect("simulator keeps running") {
            LinkEvent::Message(BridgeToHost::Rx { src, payload, .. }) => {
                let frame = Frame::decode(&payload).expect("simulator emits valid frames");
                if matches!(frame, Frame::Admin(_)) {
                    continue;
                }
                return (src, payload.to_vec());
            }
            _ => continue,
        }
    }
}

fn heartbeat_of(raw: &[u8]) -> Option<HeartbeatMsg> {
    match Frame::decode(raw) {
        Ok(Frame::Heartbeat(heartbeat)) => Some(heartbeat),
        _ => None,
    }
}

fn sighting_of(raw: &[u8]) -> Option<SightingMsg<'_>> {
    match Frame::decode(raw) {
        Ok(Frame::Sighting(sighting)) => Some(sighting),
        _ => None,
    }
}

/// Wait for the next heartbeat from `want`, returning when it arrived.
async fn next_heartbeat_from(link: &mut LinkHandle, want: Mac) -> Instant {
    loop {
        let (src, raw) = next_frame(link).await;
        if src == want && heartbeat_of(&raw).is_some() {
            return Instant::now();
        }
    }
}

fn admin_command(dst: Mac, admin: AdminMsg) -> HostToBridge {
    let mut payload = EspNowPayload::new();
    payload.extend_from_slice(&admin.encode()).expect("an assignment fits");
    HostToBridge::SendEspNow { id: 1, dst, ensure_peer: true, payload }
}

/// An assignment for node 0, since almost every test needs one.
///
/// A wartui node parks and collects nothing until told what to scan, so a test that
/// wants to see anything has to do first what the host does on the first heartbeat.
fn assign(link: &LinkHandle, version: u8, channels: ChannelSet, ble: bool) {
    link.send_urgent(admin_command(
        SimTransport::node_mac(0),
        AdminMsg {
            epoch: version,
            node_index: 0,
            node_count: 1,
            flags: AdminMsg::flags_for(ble),
            channels,
        },
    ))
    .expect("queued");
}

/// How many distinct Wi-Fi addresses are among the first `sightings` a lone node reports
/// from a neighbourhood of forty networks.
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
    while reported < sightings {
        let (_, raw) = next_frame(&mut link).await;
        if let Some(sighting) = sighting_of(&raw) {
            seen.insert(sighting.bssid);
            reported += 1;
        }
    }
    seen.len()
}

#[tokio::test(start_paused = true)]
async fn a_moving_neighbourhood_keeps_meeting_new_addresses_and_a_parked_one_does_not() {
    // Parked, forty networks fit the dedup ring, so whatever is reported again is one of
    // the same forty: the ring's refresh, not a new device.
    assert!(distinct_addresses(None, 120).await <= 40);
    // Moving on after two hearings, a network the ring suppressed once comes back under
    // an address it has never held, so every report is a device not seen before.
    assert_eq!(distinct_addresses(Some(2), 120).await, 120);
}

/// The next `SendResult` the bridge reports.
async fn next_send_result(link: &mut LinkHandle) -> SendStatus {
    loop {
        if let LinkEvent::Message(BridgeToHost::SendResult { status, .. }) =
            link.recv().await.expect("running")
        {
            return status;
        }
    }
}

/// Every channel there is, which is what a lone node on the `All` pool holds.
fn everything() -> ChannelSet {
    ChannelPool::All.channels()
}

#[tokio::test(start_paused = true)]
async fn the_bridge_announces_itself_first() {
    let mut link = SimTransport::new(SimConfig::default()).start().expect("starts");
    match link.recv().await.expect("an event") {
        LinkEvent::Connected(info) => {
            assert!(info.fw_version.ends_with("-sim"));
        }
        other => panic!("expected Connected first, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn every_node_eventually_heartbeats() {
    let config = SimConfig { node_count: 4, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    let mut seen: HashSet<Mac> = HashSet::new();
    while seen.len() < 4 {
        let (src, raw) = next_frame(&mut link).await;
        if heartbeat_of(&raw).is_some() {
            seen.insert(src);
        }
    }
    let expected: HashSet<Mac> = (0..4).map(SimTransport::node_mac).collect();
    assert_eq!(seen, expected);
}

#[tokio::test(start_paused = true)]
async fn heartbeat_counters_increase_monotonically() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    let mut counters = Vec::new();
    while counters.len() < 3 {
        if let LinkEvent::Message(BridgeToHost::Rx { payload, .. }) =
            link.recv().await.expect("running")
            && let Ok(Frame::Heartbeat(heartbeat)) = Frame::decode(&payload)
        {
            counters.push(heartbeat.counter);
        }
    }
    assert_eq!(counters, vec![1, 2, 3], "a fresh node counts up from its boot");
}

#[tokio::test(start_paused = true)]
async fn observations_decode_as_sightings() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, everything(), true);

    let mut checked = 0;
    while checked < 20 {
        let (_, raw) = next_frame(&mut link).await;
        let Some(sighting) = sighting_of(&raw) else { continue };
        assert!(sighting.rssi < 0, "signal strengths are negative dBm");
        checked += 1;
    }
}

#[tokio::test(start_paused = true)]
async fn a_node_reports_each_wifi_network_only_once() {
    // A simulator that streamed the same networks forever would leave the fleet view
    // wrong about observation rates.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, everything(), false);

    let mut counts: HashMap<[u8; 6], usize> = HashMap::new();
    let mut sweeps = 0;
    while sweeps < 3 {
        let (_, raw) = next_frame(&mut link).await;
        match Frame::decode(&raw) {
            Ok(Frame::Heartbeat(_)) => sweeps += 1,
            Ok(Frame::Sighting(sighting)) => *counts.entry(sighting.bssid).or_default() += 1,
            _ => {}
        }
    }
    assert!(!counts.is_empty(), "the fake neighbourhood should not be empty");
    let repeats: Vec<_> = counts.iter().filter(|(_, n)| **n > 1).collect();
    assert!(repeats.is_empty(), "networks reported more than once over 3 sweeps: {repeats:?}");
}

#[tokio::test(start_paused = true)]
async fn ble_sightings_keep_arriving_because_their_addresses_rotate() {
    let config = SimConfig { node_count: 1, ble_chance: 1.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, everything(), true);

    let mut ble = HashSet::new();
    let mut sweeps = 0;
    while sweeps < 3 {
        let (_, raw) = next_frame(&mut link).await;
        match Frame::decode(&raw) {
            Ok(Frame::Heartbeat(_)) => sweeps += 1,
            Ok(Frame::Sighting(sighting)) if sighting.kind == RecordKind::Ble => {
                ble.insert(sighting.bssid);
            }
            _ => {}
        }
    }
    assert!(ble.len() > 10, "rotating BLE addresses should keep producing new sightings");
}

#[tokio::test(start_paused = true)]
async fn narrowing_a_nodes_range_collapses_its_heartbeat_period() {
    // How an assignment is confirmed with no access to the node's own console.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_mac(0);
    assign(&link, 1, everything(), false);

    // Skip the sweep that straddles the first assignment, then measure.
    next_heartbeat_from(&mut link, node).await;
    let first = next_heartbeat_from(&mut link, node).await;
    let second = next_heartbeat_from(&mut link, node).await;
    let wide = second - first;

    let mut one = ChannelSet::empty();
    one.insert(0);
    assign(&link, 2, one, false);

    // Skip the sweep that straddles the change, then measure a clean one.
    next_heartbeat_from(&mut link, node).await;
    let a = next_heartbeat_from(&mut link, node).await;
    let b = next_heartbeat_from(&mut link, node).await;
    let narrow = b - a;

    assert!(
        narrow * 4 < wide,
        "a one-channel node should heartbeat far faster: {narrow:?} vs {wide:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_assignment_is_acknowledged() {
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_mac(0);

    assign(&link, 1, ChannelSet::from_run(IndexRun::new(4, 8)), false);
    let _ = node;

    loop {
        if let LinkEvent::Message(BridgeToHost::SendResult { id, status, .. }) =
            link.recv().await.expect("running")
        {
            assert_eq!(id, 1);
            assert_eq!(status, SendStatus::AckOk);
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn sending_to_an_absent_node_is_not_acknowledged() {
    // Clearing an assignment's dirty flag on this would be the firmware's bug;
    // the host must be able to tell delivery from a successful enqueue.
    let config = SimConfig { node_count: 1, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    link.send_urgent(admin_command(
        [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01],
        AdminMsg {
            epoch: 1,
            node_index: 0,
            node_count: 1,
            flags: 0,
            channels: ChannelSet::from_run(IndexRun::new(0, 3)),
        },
    ))
    .expect("queued");

    loop {
        if let LinkEvent::Message(BridgeToHost::SendResult { status, .. }) =
            link.recv().await.expect("running")
        {
            assert_eq!(status, SendStatus::AckFail);
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_repeated_assignment_version_is_ignored_by_the_node() {
    // Nodes compare with `!=`, and a node that has never heard a core holds
    // version 0. Re-sending the version it already has changes nothing, which
    // is exactly why the host must persist a monotonic counter across restarts.
    let config = SimConfig { node_count: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_mac(0);

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
async fn a_status_request_is_answered() {
    let mut link = SimTransport::new(SimConfig::default()).start().expect("starts");
    link.send_bulk(HostToBridge::SetChannel { channel: 11 }).expect("queued");
    link.send_bulk(HostToBridge::GetStatus).expect("queued");

    loop {
        if let LinkEvent::Message(BridgeToHost::Status { channel, .. }) =
            link.recv().await.expect("running")
        {
            assert_eq!(channel, 11, "the bridge should report the channel it was set to");
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_seeded_run_is_reproducible() {
    async fn first_sightings(seed: u64) -> Vec<Vec<u8>> {
        let config = SimConfig { node_count: 1, seed, ..SimConfig::default() };
        let mut link = SimTransport::new(config).start().expect("starts");
        assign(&link, 1, everything(), true);
        let mut frames = Vec::new();
        while frames.len() < 10 {
            let (_, raw) = next_frame(&mut link).await;
            if sighting_of(&raw).is_some() {
                frames.push(raw);
            }
        }
        frames
    }

    assert_eq!(first_sightings(1234).await, first_sightings(1234).await);
    assert_ne!(first_sightings(1234).await, first_sightings(9999).await);
}

#[tokio::test(start_paused = true)]
async fn only_the_node_given_the_bluetooth_assignment_reports_any() {
    let config = SimConfig { node_count: 2, ble_chance: 1.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");
    assign(&link, 1, everything(), true);
    link.send_urgent(admin_command(
        SimTransport::node_mac(1),
        AdminMsg { epoch: 1, node_index: 1, node_count: 2, flags: 0, channels: everything() },
    ))
    .expect("queued");

    // A room where an advertiser turns up on every dwell, so a node that was
    // going to report one has had every chance to.
    let mut ble_by_node: HashMap<Mac, usize> = HashMap::new();
    let mut sweeps = 0;
    while sweeps < 3 {
        let (src, raw) = next_frame(&mut link).await;
        match Frame::decode(&raw) {
            Ok(Frame::Heartbeat(_)) if src == SimTransport::node_mac(1) => sweeps += 1,
            Ok(Frame::Sighting(sighting)) if sighting.kind == RecordKind::Ble => {
                *ble_by_node.entry(src).or_default() += 1;
            }
            _ => {}
        }
    }
    assert!(ble_by_node[&SimTransport::node_mac(0)] > 5, "the node that was asked");
    assert_eq!(ble_by_node.get(&SimTransport::node_mac(1)), None, "and only that node");
}

#[tokio::test(start_paused = true)]
async fn a_simulated_c6_says_it_has_no_five_ghz_radio() {
    // The only way to put a mixed fleet in front of the planner without two kinds of
    // board on the desk.
    let config = SimConfig { node_count: 3, c6_nodes: 1, ble_chance: 0.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starts");

    let mut bands: HashMap<Mac, bool> = HashMap::new();
    while bands.len() < 3 {
        let (src, raw) = next_frame(&mut link).await;
        let Some(heartbeat) = heartbeat_of(&raw) else { continue };
        bands.insert(src, heartbeat.capabilities.five_ghz);
    }

    // Counted from the end, so the indices below the count keep their radios as
    // the fleet grows.
    assert!(bands[&SimTransport::node_mac(0)]);
    assert!(bands[&SimTransport::node_mac(1)]);
    assert!(!bands[&SimTransport::node_mac(2)], "the last one is the C6");
}

#[tokio::test(start_paused = true)]
async fn a_node_holding_the_bluetooth_antenna_acknowledges_nothing_and_cannot_be_told_to_stop() {
    // Reproduced without hardware so the host's `no admin ack` path can be
    // exercised; see `SimConfig::ble_coexistence_failure`.
    let config = SimConfig {
        node_count: 1,
        ble_coexistence_failure: true,
        ble_chance: 0.0,
        ..SimConfig::default()
    };
    let mut link = SimTransport::new(config).start().expect("starts");
    let node = SimTransport::node_mac(0);

    // The frame that switches Bluetooth on is acknowledged: the radio was not
    // yet away on the other antenna when it arrived.
    assign(&link, 1, everything(), true);
    assert_eq!(next_send_result(&mut link).await, SendStatus::AckOk);
    next_heartbeat_from(&mut link, node).await;

    // Nothing after it is. An 802.11 acknowledgement comes from the receiver's
    // MAC hardware, so this is indistinguishable from a node that is not there
    // — and the frame really is dropped, which is why the operator cannot take
    // the assignment back.
    for epoch in 2..=4 {
        assign(&link, epoch, everything(), false);
        assert_eq!(next_send_result(&mut link).await, SendStatus::AckFail, "epoch {epoch}");
    }
}
