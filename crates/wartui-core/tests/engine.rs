//! The engine's rules, checked against a clock the test invents.
//!
//! Every one of these is a rule that would otherwise only be observable by
//! watching real hardware for a minute or more — a node ageing out, a reboot,
//! a status poll falling due. Being able to assert them in microseconds is the
//! whole reason the engine reads no clock of its own.

use std::time::{Duration, Instant};

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_core::engine::{Command, Counters, EngineConfig, Event, FleetEngine, Now, StoreStats};
use wartui_core::position::{PositionChain, PositionSource};
use wartui_core::record::{AdminOutcome, Record};
use wartui_proto::air::{AdminMsg, MsgType, TextMsg};
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, EspNowPayload, HostToBridge, Mac, SendStatus,
};
use wartui_proto::plan::IndexRun;

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
    rx_at(src, msg_type, counter, text, 0)
}

/// A frame carrying the bridge's own microsecond stamp, which is the near end
/// of every assignment-latency measurement.
fn rx_at(src: Mac, msg_type: MsgType, counter: u32, text: &[u8], rx_us: u32) -> Event {
    let frame = TextMsg::new(msg_type, counter, text).expect("fits").encode();
    Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src,
        dst: BROADCAST,
        rssi: -41,
        channel: 6,
        rx_us,
        payload: EspNowPayload::from_slice(&frame).expect("fits"),
    }))
}

fn heartbeat(src: Mac, counter: u32) -> Event {
    rx(src, MsgType::Heartbeat, counter, b"")
}

fn send_result(id: u16, status: SendStatus, tx_us: u32) -> Event {
    Event::Link(LinkEvent::Message(BridgeToHost::SendResult { id, status, tx_us }))
}

/// The one assignment in a batch, decoded back off the wire.
fn sent_admin(batch: &wartui_core::ActionBatch) -> (u16, Mac, AdminMsg) {
    assert_eq!(batch.urgent.len(), 1, "exactly one assignment");
    let HostToBridge::SendEspNow { id, dst, ensure_peer, payload } = &batch.urgent[0] else {
        panic!("expected a SendEspNow")
    };
    assert!(ensure_peer, "divergence 6: peers are added on demand and never deleted");
    (*id, *dst, AdminMsg::decode(payload).expect("a valid admin frame"))
}

fn assign(mac: Mac, start: u8, end: u8) -> Event {
    Event::Command(Command::Assign { mac, range: IndexRun::new(start, end) })
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
    // But it is not one of ours. Admitting it would leave a rival core sitting
    // in the fleet table forever as `no heartbeat`, inflating the denominator
    // of "n of m alive" and leaving a `node` row that outlives the session.
    assert_eq!(engine.nodes().count(), 0);
}

#[test]
fn a_sender_whose_frames_do_not_decode_does_not_join_the_fleet() {
    // Channel 6 carries whatever else is nearby. A transmitter we cannot
    // understand is counted, not adopted.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let event = Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src: OTHER,
        dst: BROADCAST,
        rssi: -41,
        channel: 6,
        rx_us: 0,
        payload: EspNowPayload::from_slice(b"not an ENOW frame").expect("fits"),
    }));
    let batch = engine.handle(event, clock.at(1));

    assert_eq!(counters(&engine).undecodable, 1);
    assert_eq!(engine.nodes().count(), 0);
    assert!(batch.records.is_empty());
}

#[test]
fn the_bridge_is_recorded_when_it_announces_itself() {
    // The session row is written before any bridge has announced, so this is
    // the only chance to say which dongle produced the capture.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let batch = engine.handle(connected(), clock.at(1));

    let Some(Record::Bridge(bridge)) = batch.records.first() else {
        panic!("expected a bridge record, got {:?}", batch.records)
    };
    assert_eq!(bridge.mac, [0x98, 0xA3, 0x16, 0x8E, 0x9D, 0x24]);
    assert_eq!(bridge.chip, "Esp32C6");
    assert_eq!(bridge.fw_version, "0.1.0");
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

// ---------------------------------------------------------------------------
// Phase 4: assignment
// ---------------------------------------------------------------------------

#[test]
fn an_assignment_waits_for_the_heartbeat_that_opens_the_window() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    // Nothing goes out at the moment the operator asks. A node's radio is away
    // scanning some other channel for all but the 300 ms after its own
    // heartbeat, so a frame sent now would be transmitted into silence.
    let asked = engine.handle(assign(NODE, 5, 5), clock.at(2));
    assert!(asked.urgent.is_empty(), "nothing is sent until the window opens");

    let opened = engine.handle(heartbeat(NODE, 2), clock.at(6));
    let (_, dst, admin) = sent_admin(&opened);
    assert_eq!(dst, NODE);
    assert_eq!((admin.start_channel_idx, admin.end_channel_idx), (5, 5));
    assert_eq!(admin.assignment_version, 1, "the first epoch of a fresh database");
    assert_eq!(counters(&engine).admin_sent, 1);
}

#[test]
fn an_assignment_is_believed_only_once_the_node_radio_acknowledges_it() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 0, 10), clock.at(2));
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // Divergence 3. The vendor core clears its dirty flag from the
    // `esp_now_send` return value, so an enqueue that nothing received still
    // counts as an assignment delivered.
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(6));
    let node = engine.nodes().next().expect("the node");
    assert!(!node.dirty, "acknowledged, so there is nothing left to deliver");
    assert_eq!(node.confirmed.expect("confirmed").range, IndexRun::new(0, 10));
    assert_eq!(counters(&engine).admin_acked, 1);

    let Some(Record::Assignment(row)) = batch.records.first() else {
        panic!("every attempt is written down")
    };
    assert_eq!(row.outcome, AdminOutcome::Acked);
    assert_eq!((row.start_idx, row.end_idx), (0, 10));

    // And a later heartbeat sends nothing, because there is nothing to send.
    assert!(engine.handle(heartbeat(NODE, 3), clock.at(10)).urgent.is_empty());
}

#[test]
fn an_unacknowledged_assignment_is_retried_on_the_next_heartbeat() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 3, 3), clock.at(2));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // The observed cause is BLE coexistence on the node: NimBLE holds the one
    // 2.4 GHz antenna through exactly the window the assignment arrives in, so
    // the node's MAC never acknowledges. A miss costs one sweep, not the whole
    // capture.
    let batch = engine.handle(send_result(id, SendStatus::AckFail, 900), clock.at(6));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Unacked);
    assert!(engine.nodes().next().expect("the node").dirty, "still owed");

    let (_, _, retry) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(10)));
    assert_eq!(
        retry.assignment_version, first.assignment_version,
        "a retry of the same assignment keeps its epoch; the node never saw it"
    );
    assert_eq!(counters(&engine).admin_failed, 1);
}

#[test]
fn an_assignment_the_bridge_never_answers_for_is_written_down_as_unknown() {
    let clock = Clock::new();
    let config = EngineConfig { admin_timeout: Duration::from_secs(2), ..Default::default() };
    let mut engine = engine(config, &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 3, 3), clock.at(2));
    sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    assert!(engine.handle(Event::Tick, clock.at(7)).records.is_empty(), "still in flight");

    let batch = engine.handle(Event::Tick, clock.at(9));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    // Distinct from `Unacked`, where the radio did report back. Here the bridge
    // itself went quiet, and the difference points at different hardware.
    assert_eq!(row.outcome, AdminOutcome::Silent);
    assert_eq!(row.latency_us, None);
    // Nothing arrived, so there is no delivery time. Stamping one would record
    // the timeout's own length dressed up as an answer.
    assert_eq!(row.delivered_at_ms, None);
    assert!(engine.nodes().next().expect("the node").dirty);
}

#[test]
fn a_lost_answer_expiring_late_does_not_unpick_an_assignment_that_has_since_landed() {
    let clock = Clock::new();
    let config = EngineConfig { admin_timeout: Duration::from_secs(2), ..Default::default() };
    let mut engine = engine(config, &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 5, 5), clock.at(2));

    // The first attempt's answer is lost on the way back — a garbled USB frame,
    // which the live capture logged happening.
    let (first, ..) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(3)));
    // So the node is still dirty on its next heartbeat and the retry goes out,
    // under the same epoch, and this one is acknowledged.
    let (second, ..) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(4)));
    assert_ne!(first, second);
    engine.handle(send_result(second, SendStatus::AckOk, 4_000), clock.at(4));

    let node = engine.nodes().next().expect("the node").clone();
    assert_eq!(node.confirmed.expect("confirmed").range, IndexRun::new(5, 5));

    // Now the stranded first attempt times out. It earns its row — that attempt
    // really did go unanswered — but the node is demonstrably holding the
    // range, and the view must not start claiming otherwise.
    let batch = engine.handle(Event::Tick, clock.at(5));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Silent);

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.last_outcome, Some(AdminOutcome::Acked));
    assert_eq!(node.confirmed.expect("still confirmed").range, IndexRun::new(5, 5));
    assert!(!node.dirty, "nothing is owed: the node acknowledged this epoch");
}

#[test]
fn the_assignment_latency_is_measured_end_to_end_on_the_bridge_clock() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 7, 7), clock.at(2));

    // The heartbeat is stamped by the bridge at 1_000_000 µs of its own uptime
    // and the transmit callback at 1_004_500: 4.5 ms from "the node is
    // listening" to "its radio says it has the frame", with none of the host's
    // scheduling in between.
    let opened = engine.handle(rx_at(NODE, MsgType::Heartbeat, 2, b"", 1_000_000), clock.at(6));
    let (id, _, _) = sent_admin(&opened);
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 1_004_500), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, Some(4_500));
    assert_eq!(engine.nodes().next().expect("the node").last_latency_us, Some(4_500));
}

#[test]
fn a_latency_measured_across_the_bridge_counter_wrapping_is_still_right() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 7, 7), clock.at(2));

    // The bridge's stamp is a `u32` of microseconds and so wraps every 71
    // minutes or so. A heartbeat 1 ms before the wrap and a callback 3.5 ms
    // after it are 4.5 ms apart, and a wrapping subtraction says so.
    let opened =
        engine.handle(rx_at(NODE, MsgType::Heartbeat, 2, b"", u32::MAX - 999), clock.at(6));
    let (id, _, _) = sent_admin(&opened);
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 3_500), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, Some(4_500));
}

#[test]
fn a_reboot_re_issues_the_assignment_under_a_fresh_epoch() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 40), clock.at(1));
    engine.handle(assign(NODE, 2, 4), clock.at(2));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 41), clock.at(6)));
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(6));
    assert!(engine.nodes().next().expect("the node").confirmed.is_some());

    // Divergence 5: the counter runs from the node's boot, so a value below the
    // last one means it restarted and has forgotten its range. Its own version
    // field went back to a boot value with it, and since a node adopts on `!=`
    // rather than `>`, re-sending the old epoch could match what it now holds
    // and be discarded — while still being acknowledged.
    let batch = engine.handle(heartbeat(NODE, 1), clock.at(10));
    let (_, _, reissued) = sent_admin(&batch);
    assert_ne!(reissued.assignment_version, first.assignment_version);
    assert_eq!((reissued.start_channel_idx, reissued.end_channel_idx), (2, 4));

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.reboots, 1);
    assert!(node.confirmed.is_none(), "what it held is no longer believed");
}

#[test]
fn epochs_carry_on_from_where_the_database_left_off() {
    let clock = Clock::new();
    // Divergence 4. The vendor core keeps this counter in RAM and resets it to
    // 1 at boot, so a restarted core that recomputes an assignment a node
    // already holds is ignored — and if the topology changed while it was
    // down, the two views never reconcile.
    let config = EngineConfig { assignment_base: 300, ..Default::default() };
    let mut engine = engine(config, &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 1, 1), clock.at(2));

    let (_, _, admin) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    // 301 narrowed to a byte that skips zero, which the firmware never sends.
    assert_eq!(admin.assignment_version, 46);
}

#[test]
fn the_fleet_arithmetic_travels_with_the_assignment_rather_than_being_read_at_send_time() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 0, 10), clock.at(2));

    // Divergence 7: a node joining between the plan and the send must not
    // change the count the earlier node is told, or that node computes its
    // transmit stagger slot against a fleet size its own range never came from.
    engine.handle(heartbeat(OTHER, 1), clock.at(3));

    let (_, _, admin) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    assert_eq!(admin.node_count, 1, "the fleet as it was when the range was cut");
    assert_eq!(admin.node_index, 0);
}

#[test]
fn a_node_that_has_never_been_heard_from_cannot_be_assigned() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let batch = engine.handle(assign(NODE, 0, 0), clock.at(1));

    assert!(batch.urgent.is_empty());
    assert_eq!(engine.nodes().count(), 0, "asking about a node does not invent one");
}

#[test]
fn the_heartbeat_period_is_the_median_of_recent_sweeps() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    // Four-second sweeps, which is what a node scanning all forty channels
    // does, with one heartbeat lost in the middle. The median is what keeps
    // that lost beat from reading as a range twice the size.
    for (n, at) in [1u64, 5, 9, 17, 21, 25].into_iter().enumerate() {
        engine.handle(heartbeat(NODE, n as u32 + 1), clock.at(at));
    }

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.beat_period_ms(), Some(4_000));
}

#[test]
fn a_bridge_that_cannot_transmit_at_all_is_a_failure_not_a_delivery() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 0, 0), clock.at(2));
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    let batch = engine.handle(send_result(id, SendStatus::Rejected, 900), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Refused);
    // Still owed: a bridge that refused one frame may take the next one.
    assert!(engine.nodes().next().expect("the node").dirty);
    assert_eq!(counters(&engine).admin_acked, 0);
}

#[test]
fn a_fleet_too_big_for_the_radio_is_told_so_once_rather_than_retried_forever() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 0, 0), clock.at(2));
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    let batch = engine.handle(send_result(id, SendStatus::PeerTableFull, 900), clock.at(6));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Refused);

    // Twenty peers is the radio's whole table and twenty nodes is the whole
    // supported fleet, so no later heartbeat makes this possible. Retrying
    // would write one failure row per sweep for the rest of the capture.
    let node = engine.nodes().next().expect("the node");
    assert!(!node.dirty, "nothing is owed to a node the bridge cannot address");
    assert!(node.desired.is_none(), "and nothing is still wanted");
    assert_eq!(counters(&engine).peer_table_full, 1);

    let next = engine.handle(heartbeat(NODE, 3), clock.at(10));
    assert!(next.urgent.is_empty(), "and the next sweep does not try again");
}

#[test]
fn an_acknowledgement_for_an_assignment_the_operator_has_already_replaced_is_not_believed() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 0, 0), clock.at(2));
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // The operator changed their mind while the first frame was in flight. The
    // ack that arrives afterwards is for a range nobody wants any more, and
    // treating it as current would leave the fleet table describing an
    // assignment that was superseded before it landed.
    engine.handle(assign(NODE, 5, 9), clock.at(6));
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(7));

    let node = engine.nodes().next().expect("the node");
    assert!(node.dirty, "the newer assignment is still owed");
    assert!(node.confirmed.is_none());
    assert_eq!(node.desired.expect("desired").range, IndexRun::new(5, 9));
}
