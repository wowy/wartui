//! The engine's rules, checked against a clock the test invents.
//!
//! Every one of these is a rule that would otherwise only be observable by
//! watching real hardware for a minute or more — a node ageing out, a reboot,
//! a status poll falling due. Being able to assert them in microseconds is the
//! whole reason the engine reads no clock of its own.

use std::time::{Duration, Instant};

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_core::engine::{Counters, EngineConfig, Event, FleetEngine, Now, StoreStats};
use wartui_core::position::{PositionChain, PositionSource};
use wartui_core::record::Record;
use wartui_proto::air::{MsgType, TextMsg};
use wartui_proto::link::{BROADCAST, BridgeToHost, Chip, EspNowPayload, Mac};

const NODE: Mac = [0x38, 0x44, 0xBE, 0x1F, 0x57, 0x84];
const OTHER: Mac = [0x38, 0x44, 0xBE, 0x1F, 0x57, 0x85];
const EPOCH_MS: i64 = 1_777_642_477_000;

/// A clock the test drives by hand.
struct Clock {
    base: Instant,
}

impl Clock {
    fn new() -> Self {
        Self { base: Instant::now() }
    }

    fn at(&self, secs: u64) -> Now {
        Now {
            mono: self.base + Duration::from_secs(secs),
            unix_ms: EPOCH_MS + i64::try_from(secs).expect("test clock fits") * 1000,
        }
    }
}

fn engine(config: EngineConfig, clock: &Clock) -> FleetEngine {
    FleetEngine::new(config, clock.at(0))
}

fn rx(src: Mac, msg_type: MsgType, counter: u32, text: &[u8]) -> Event {
    let frame = TextMsg::new(msg_type, counter, text).expect("fits").encode();
    Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src,
        dst: BROADCAST,
        rssi: -41,
        channel: 6,
        rx_us: 0,
        payload: EspNowPayload::from_slice(&frame).expect("fits"),
    }))
}

fn observation(src: Mac, bssid: &str, rssi: i16) -> Event {
    let line = format!("{bssid},example,[WPA2_PSK],6,{rssi},W");
    rx(src, MsgType::Text, 0, line.as_bytes())
}

fn connected() -> Event {
    Event::Link(LinkEvent::Connected(BridgeInfo {
        chip: Chip::Esp32C6,
        mac: [0x98, 0xA3, 0x16, 0x8E, 0x9D, 0x24],
        fw_version: "0.1.0".to_owned(),
    }))
}

fn counters(engine: &FleetEngine) -> Counters {
    engine.counters()
}

#[test]
fn an_observation_produces_a_node_row_and_an_observation_row() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));

    assert_eq!(batch.records.len(), 2, "one node touch and one observation");
    assert!(matches!(batch.records[0], Record::Node(_)));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };
    assert_eq!(obs.bssid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    assert_eq!(obs.rssi, -60);
    assert_eq!(obs.security, "[WPA2_PSK]");
    assert_eq!(obs.rx_at_ms, EPOCH_MS + 1000);
    assert_eq!(obs.link_rssi, Some(-41), "how well the bridge heard the node, not the network");
    assert_eq!(counters(&engine).observations, 1);
}

#[test]
fn the_raw_line_is_kept_alongside_the_parsed_one() {
    // The wire format is undocumented and read out of someone else's C++. If
    // this decoder turns out to be wrong, the raw text is what lets the fix be
    // applied to history rather than only to what arrives afterwards.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };

    assert_eq!(obs.raw_text, b"AA:BB:CC:DD:EE:FF,example,[WPA2_PSK],6,-60,W");
}

#[test]
fn a_heartbeat_counter_going_backwards_counts_a_reboot() {
    // Divergence 5. The counter runs from the node's boot, so a regression
    // means it restarted and has forgotten whatever range it was assigned.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(rx(NODE, MsgType::Heartbeat, 174, b""), clock.at(1));
    engine.handle(rx(NODE, MsgType::Heartbeat, 175, b""), clock.at(2));
    assert_eq!(engine.nodes().next().expect("a node").reboots, 0);

    engine.handle(rx(NODE, MsgType::Heartbeat, 2, b""), clock.at(3));

    let node = engine.nodes().next().expect("a node");
    assert_eq!(node.reboots, 1);
    assert_eq!(node.counter, Some(2), "the new counter is adopted, not rejected");
}

#[test]
fn observations_keep_a_node_visible_but_only_heartbeats_keep_it_assignable() {
    // Divergence 2. The vendor firmware's `touchNode` call on the text path is
    // commented out (`src/WiFiOps.cpp:1073-1082`), so a node streaming data
    // whose heartbeats are being lost ages out and churns the fleet's whole
    // topology. Two clocks, because they answer different questions: is this
    // node there, and can it still be given a channel range.
    let clock = Clock::new();
    let config = EngineConfig { topology_timeout: Duration::from_secs(60), ..Default::default() };
    let mut engine = engine(config, &clock);

    engine.handle(rx(NODE, MsgType::Heartbeat, 1, b""), clock.at(1));
    for second in 2..120 {
        engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(second));
    }

    let now = clock.at(120);
    let node = engine.nodes().next().expect("a node").clone();
    assert_eq!(node.last_seen_ms, EPOCH_MS + 119_000, "still being heard");
    assert!(!engine.is_alive(&node, now), "but not heartbeating, so not assignable");

    engine.handle(rx(NODE, MsgType::Heartbeat, 2, b""), clock.at(121));
    let node = engine.nodes().next().expect("a node").clone();
    assert!(engine.is_alive(&node, clock.at(122)));
}

#[test]
fn a_malformed_line_is_counted_rather_than_stored() {
    // A node emitting these is broken in a way silence would not distinguish,
    // and writing the rubbish into the store would corrupt the export.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let batch = engine.handle(rx(NODE, MsgType::Text, 0, b"not,enough,fields"), clock.at(1));

    assert_eq!(counters(&engine).unparsed, 1);
    assert_eq!(counters(&engine).observations, 0);
    assert!(
        batch.records.iter().all(|r| matches!(r, Record::Node(_))),
        "the node was still heard from, but nothing was recorded as an observation"
    );
}

#[test]
fn a_frame_that_is_not_enow_at_all_is_counted_as_undecodable() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let event = Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src: NODE,
        dst: BROADCAST,
        rssi: -41,
        channel: 6,
        rx_us: 0,
        payload: EspNowPayload::from_slice(b"not an ENOW frame").expect("fits"),
    }));
    engine.handle(event, clock.at(1));

    assert_eq!(counters(&engine).undecodable, 1);
    assert_eq!(counters(&engine).frames, 1);
}

#[test]
fn a_core_frame_marks_the_node_as_still_encrypted() {
    // wartui does not speak encrypted ESP-NOW at all. Naming the node is what
    // turns an unexplained silence into "turn encryption off on this one".
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(rx(NODE, MsgType::CoreRequest, 0, b""), clock.at(1));

    assert!(engine.nodes().next().expect("a node").encrypted);
    assert_eq!(counters(&engine).core_frames, 1);
}

#[test]
fn an_admin_frame_from_elsewhere_means_a_rival_core_is_powered_up() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let admin = wartui_proto::air::AdminMsg {
        assignment_version: 3,
        node_index: 0,
        node_count: 2,
        start_channel_idx: 0,
        end_channel_idx: 19,
    }
    .encode();
    let event = Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src: OTHER,
        dst: NODE,
        rssi: -41,
        channel: 6,
        rx_us: 0,
        payload: EspNowPayload::from_slice(&admin).expect("fits"),
    }));
    engine.handle(event, clock.at(1));

    assert_eq!(counters(&engine).foreign_admin, 1);
}

#[test]
fn the_bridge_is_asked_for_its_counters_on_connect_and_then_on_the_interval() {
    let clock = Clock::new();
    let config = EngineConfig { status_interval: Duration::from_secs(5), ..Default::default() };
    let mut engine = engine(config, &clock);

    let batch = engine.handle(connected(), clock.at(1));
    assert_eq!(batch.bulk, vec![wartui_proto::link::HostToBridge::GetStatus]);

    // Not again straight away: the interval starts from the connect.
    assert!(engine.handle(Event::Tick, clock.at(3)).bulk.is_empty());
    assert_eq!(
        engine.handle(Event::Tick, clock.at(6)).bulk,
        vec![wartui_proto::link::HostToBridge::GetStatus]
    );
}

#[test]
fn a_disconnected_bridge_is_not_polled() {
    // Every poll issued at a closed link is a command queued to be thrown away
    // on reconnect, and a `dropping a bulk command` log line for the operator
    // to wonder about.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(connected(), clock.at(1));
    engine.handle(Event::Link(LinkEvent::Disconnected { reason: "cable".to_owned() }), clock.at(2));

    assert!(engine.handle(Event::Tick, clock.at(60)).bulk.is_empty());
}

#[test]
fn the_tail_keeps_the_newest_observations_and_no_more() {
    let clock = Clock::new();
    let config = EngineConfig { tail_len: 4, ..Default::default() };
    let mut engine = engine(config, &clock);

    for n in 0..10u8 {
        let bssid = format!("AA:BB:CC:DD:EE:{n:02X}");
        engine.handle(observation(NODE, &bssid, -60), clock.at(u64::from(n) + 1));
    }

    let snapshot = engine.snapshot(clock.at(11), StoreStats::default());
    assert_eq!(snapshot.tail.len(), 4);
    assert_eq!(snapshot.tail[0].bssid[5], 6, "oldest kept");
    assert_eq!(snapshot.tail[3].bssid[5], 9, "newest last");
}

#[test]
fn every_observation_carries_the_position_the_host_believed_in() {
    let clock = Clock::new();
    let config = EngineConfig {
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0)),
        ..Default::default()
    };
    let mut engine = engine(config, &clock);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };

    assert_eq!(obs.fix.source, PositionSource::Static);
    assert_eq!(obs.fix.lat, Some(37.7749));
}

#[test]
fn raw_frames_are_only_kept_when_asked_for() {
    let clock = Clock::new();
    let mut off = engine(EngineConfig::default(), &clock);
    let mut on = engine(EngineConfig { record_raw: true, ..Default::default() }, &clock);

    let has_raw = |batch: &wartui_core::ActionBatch| {
        batch.records.iter().any(|r| matches!(r, Record::Raw(_)))
    };
    assert!(!has_raw(&off.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1))));
    assert!(has_raw(&on.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1))));
}

#[test]
fn nodes_are_ordered_by_mac_so_the_table_does_not_reshuffle() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(rx(OTHER, MsgType::Heartbeat, 1, b""), clock.at(1));
    engine.handle(rx(NODE, MsgType::Heartbeat, 1, b""), clock.at(2));

    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    let macs: Vec<Mac> = snapshot.nodes.iter().map(|n| n.state.mac).collect();
    assert_eq!(macs, vec![NODE, OTHER]);
    assert_eq!(snapshot.alive, 2);
}

#[test]
fn bridge_drops_are_counted_from_the_moment_this_host_attached() {
    // A bridge left powered with nothing attached drops everything it hears,
    // so its own counter is dominated by history the operator cannot act on.
    // What matters is whether *this* capture lost anything.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let status = |dropped_tx| {
        Event::Link(LinkEvent::Message(BridgeToHost::Status {
            channel: 6,
            peer_count: 1,
            rx_count: 1363,
            dropped_tx,
            uptime_ms: 3_240_000,
        }))
    };

    engine.handle(connected(), clock.at(1));
    engine.handle(status(1300), clock.at(2));
    let seen = |engine: &FleetEngine| {
        engine.snapshot(clock.at(9), StoreStats::default()).bridge_status.expect("a status")
    };
    assert_eq!(seen(&engine).dropped_since_attach, 0, "1300 of them predate us");
    assert_eq!(seen(&engine).dropped_tx, 1300, "the absolute count is still available");

    engine.handle(status(1305), clock.at(7));
    assert_eq!(seen(&engine).dropped_since_attach, 5, "these five are ours");

    // The bridge restarted and began counting again from zero, so the old
    // baseline would otherwise underflow or hide real losses.
    engine.handle(status(2), clock.at(8));
    assert_eq!(seen(&engine).dropped_since_attach, 0);
    engine.handle(status(9), clock.at(9));
    assert_eq!(seen(&engine).dropped_since_attach, 7);
}
