//! The engine's rules, checked against a clock the test invents.
//!
//! Every one of these is a rule that would otherwise only be observable by
//! watching real hardware for a minute or more — a node ageing out, a reboot,
//! a status poll falling due. Being able to assert them in microseconds is the
//! whole reason the engine reads no clock of its own.

use std::time::{Duration, Instant};

use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_core::engine::{Command, Counters, EngineConfig, Event, FleetEngine, Now, StoreStats};
use wartui_core::gps::Gps;
use wartui_core::position::{DEFAULT_MAX_AGE, PositionChain, PositionSource};
use wartui_core::record::{AdminOutcome, Record};
use wartui_proto::air::{
    AdminMsg, Capabilities, HeartbeatMsg, RecordKind, SIGHTING_MSG_MAX, Security, SightingMsg,
};
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, EspNowPayload, HostToBridge, LoopPhase, Mac, ResetCause,
    SendStatus,
};
use wartui_proto::plan::{
    ChannelPool, ChannelSet, FIRST_FIVE_GHZ_INDEX, IndexRun, is_five_ghz, plan,
};

const NODE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
const OTHER: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x85];
const THIRD: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x86];
/// Sorts before every `peer(n)`, so counting it would shift all their indices.
const GONE: Mac = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
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
        self.at_ms(secs * 1000)
    }

    /// Millisecond resolution, for the tests that care how fast the host is
    /// reading the link rather than how much time has passed.
    fn at_ms(&self, ms: u64) -> Now {
        Now {
            mono: self.base + Duration::from_millis(ms),
            unix_ms: EPOCH_MS + i64::try_from(ms).expect("test clock fits"),
        }
    }
}

fn engine(config: EngineConfig, clock: &Clock) -> FleetEngine {
    FleetEngine::new(config, clock.at(0))
}

/// Whatever `frame` is, arriving from `src`.
fn rx(src: Mac, frame: &[u8]) -> Event {
    rx_at(src, frame, 0)
}

/// A frame carrying the bridge's own microsecond stamp, which is the near end
/// of every assignment-latency measurement.
fn rx_at(src: Mac, frame: &[u8], rx_us: u32) -> Event {
    Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src,
        dst: BROADCAST,
        rssi: -41,
        channel: 6,
        rx_us,
        payload: EspNowPayload::from_slice(frame).expect("fits"),
    }))
}

/// A heartbeat from a node with everything: both features, both bands. Almost
/// every test below wants this one.
fn heartbeat(src: Mac, counter: u32) -> Event {
    beat_at(src, counter, Capabilities::here(true, true), 0)
}

/// A heartbeat from a node that can do neither of the things capabilities can
/// claim: an ESP32-C6 built without the `ble` feature.
fn narrowband_heartbeat(src: Mac, counter: u32) -> Event {
    beat_at(src, counter, Capabilities::here(false, false), 0)
}

fn beat_at(src: Mac, counter: u32, capabilities: Capabilities, rx_us: u32) -> Event {
    rx_at(src, &HeartbeatMsg { counter, capabilities }.encode(), rx_us)
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
    Event::Command(Command::Assign { mac, channels: run(start, end) })
}

/// The channels of an inclusive index run, which is what most of these tests
/// want: a set says more than a range can, but a range still reads better in a
/// test that is not about the difference.
fn run(start: u8, end: u8) -> ChannelSet {
    ChannelSet::from_run(IndexRun::new(start, end))
}

fn observation(src: Mac, bssid: &str, rssi: i8) -> Event {
    let mut raw = [0u8; 6];
    for (byte, hex) in raw.iter_mut().zip(bssid.split(':')) {
        *byte = u8::from_str_radix(hex, 16).expect("a hex octet");
    }
    let mut frame = [0u8; SIGHTING_MSG_MAX];
    let len = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: raw,
        channel: 6,
        rssi,
        security: Security::Wpa2Psk,
        ssid: b"example",
    }
    .encode_into(&mut frame)
    .expect("fits");
    rx(src, &frame[..len])
}

fn connected() -> Event {
    Event::Link(LinkEvent::Connected(BridgeInfo {
        chip: Chip::Esp32C6,
        mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
        fw_version: "0.1.0".to_owned(),
        // An ordinary connect. The engine draws no conclusions from these —
        // they are the view's business — but a fixture that said otherwise
        // would be describing a fleet that had just lost its bridge.
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        heap_free: 65_536,
        uptime_ms: 1_000,
    }))
}

fn counters(engine: &FleetEngine) -> Counters {
    engine.counters()
}

#[test]
fn an_observation_produces_a_node_row_and_an_observation_row() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

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
fn the_raw_frame_is_kept_alongside_the_parsed_one() {
    // If this decoder turns out to be wrong, the frame as it arrived is what
    // lets the fix be applied to history rather than only to what comes
    // afterwards. It carries its own version byte, so a file spanning a format
    // change stays readable row by row.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };

    assert_eq!(&obs.raw_body[..4], b"WTUI");
    assert_eq!(
        wartui_proto::air::SightingMsg::decode(&obs.raw_body).expect("valid").ssid,
        b"example"
    );
}

#[test]
fn a_heartbeat_counter_going_backwards_counts_a_reboot() {
    // Divergence 5. The counter runs from the node's boot, so a regression
    // means it restarted and has forgotten whatever range it was assigned.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    engine.handle(heartbeat(NODE, 174), clock.at(1));
    engine.handle(heartbeat(NODE, 175), clock.at(2));
    assert_eq!(engine.nodes().next().expect("a node").reboots, 0);

    engine.handle(heartbeat(NODE, 2), clock.at(3));

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
    let config = EngineConfig { topology_timeout: Duration::from_secs(60), ..manual() };
    let mut engine = engine(config, &clock);

    engine.handle(heartbeat(NODE, 1), clock.at(1));
    for second in 2..120 {
        engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(second));
    }

    let now = clock.at(120);
    let node = engine.nodes().next().expect("a node").clone();
    assert_eq!(node.last_seen_ms, EPOCH_MS + 119_000, "still being heard");
    assert!(!engine.is_alive(&node, now), "but not heartbeating, so not assignable");

    engine.handle(heartbeat(NODE, 2), clock.at(121));
    let node = engine.nodes().next().expect("a node").clone();
    assert!(engine.is_alive(&node, clock.at(122)));
}

#[test]
fn a_truncated_sighting_is_counted_rather_than_stored() {
    // A frame that arrives short is not an observation of anything, and
    // writing what did arrive would file an access point under a name it
    // never had.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    let Event::Link(LinkEvent::Message(BridgeToHost::Rx { payload, .. })) =
        observation(NODE, "AA:BB:CC:DD:EE:FF", -60)
    else {
        panic!("an Rx event")
    };
    let batch = engine.handle(rx(NODE, &payload[..payload.len() - 1]), clock.at(1));

    assert_eq!(counters(&engine).undecodable, 1);
    assert_eq!(counters(&engine).observations, 0);
    assert!(batch.records.is_empty(), "and nothing was recorded at all");
}

#[test]
fn a_frame_that_is_not_ours_at_all_is_counted_as_undecodable() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    let event = Event::Link(LinkEvent::Message(BridgeToHost::Rx {
        src: NODE,
        dst: BROADCAST,
        rssi: -41,
        channel: 6,
        rx_us: 0,
        payload: EspNowPayload::from_slice(b"not a wartui frame").expect("fits"),
    }));
    engine.handle(event, clock.at(1));

    assert_eq!(counters(&engine).undecodable, 1);
    assert_eq!(counters(&engine).frames, 1);
}

#[test]
fn a_vendor_fleet_nearby_is_counted_and_never_joins_this_one() {
    // The whole reason wartui stopped speaking the vendor's format. A stock
    // node broadcasts on this channel and used to be indistinguishable from
    // one of ours right up until it was asked what it was; now its frames do
    // not decode here at all, and ours do not decode there — so neither fleet
    // can be planned around the other. It is still counted, because a second
    // fleet transmitting where these nodes listen is worth saying out loud.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    // A stock node's heartbeat, counter 147, exactly as one came off the air.
    let vendor = [0x45, 0x4E, 0x4F, 0x57, 0x03, 0x93, 0x00, 0x00, 0x00];
    let batch = engine.handle(rx(OTHER, &vendor), clock.at(1));

    assert_eq!(counters(&engine).foreign_fleet, 1);
    assert_eq!(counters(&engine).undecodable, 0, "it is not line noise");
    assert_eq!(engine.nodes().count(), 0, "and it is not a node of ours");
    assert!(batch.records.is_empty());
}

#[test]
fn a_node_on_older_firmware_is_named_rather_than_left_as_noise() {
    // A fleet half-way through a reflash looks like this and nothing else
    // would say so: the frames are ours, from a build speaking a wire version
    // this host does not. The header carries a version precisely so that this
    // is a sentence in the fault box rather than a fleet that went quiet.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    let mut frame =
        HeartbeatMsg { counter: 1, capabilities: Capabilities::here(true, true) }.encode().to_vec();
    frame[4] = frame[4].wrapping_add(1);
    engine.handle(rx(NODE, &frame), clock.at(1));

    assert_eq!(counters(&engine).incompatible, 1);
    assert_eq!(counters(&engine).undecodable, 0);
    assert_eq!(engine.nodes().count(), 0, "it cannot be driven, so it is not in the table");
}

#[test]
fn an_admin_frame_from_elsewhere_means_a_rival_core_is_powered_up() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    let admin = wartui_proto::air::AdminMsg {
        epoch: 3,
        node_index: 0,
        node_count: 2,
        flags: 0,
        channels: run(0, 19),
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
    let mut engine = engine(manual(), &clock);

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
    let mut engine = engine(manual(), &clock);

    let batch = engine.handle(connected(), clock.at(1));

    let Some(Record::Bridge(bridge)) = batch.records.first() else {
        panic!("expected a bridge record, got {:?}", batch.records)
    };
    assert_eq!(bridge.mac, [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24]);
    assert_eq!(bridge.chip, "Esp32C6");
    assert_eq!(bridge.fw_version, "0.1.0");
}

#[test]
fn the_bridge_is_asked_for_its_counters_on_connect_and_then_on_the_interval() {
    let clock = Clock::new();
    let config = EngineConfig { status_interval: Duration::from_secs(5), ..manual() };
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
    let mut engine = engine(manual(), &clock);

    engine.handle(connected(), clock.at(1));
    engine.handle(Event::Link(LinkEvent::Disconnected { reason: "cable".to_owned() }), clock.at(2));

    assert!(engine.handle(Event::Tick, clock.at(60)).bulk.is_empty());
}

#[test]
fn the_tail_keeps_the_newest_observations_and_no_more() {
    let clock = Clock::new();
    let config = EngineConfig { tail_len: 4, ..manual() };
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
    let config =
        EngineConfig { position: PositionChain::fixed(37.7749, -122.4194, Some(16.0)), ..manual() };
    let mut engine = engine(config, &clock);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };

    assert_eq!(obs.fix.source, PositionSource::Static);
    assert_eq!(obs.fix.lat, Some(37.7749));
}

/// A GGA fix at 48.1173N, 11.5167E — somewhere very much not San Francisco,
/// so which tier answered is visible in the coordinates alone.
const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";

/// The observation in a batch, wherever the node-seen row landed relative to it.
fn observed(batch: &wartui_core::ActionBatch) -> &wartui_core::record::Observation {
    batch
        .records
        .iter()
        .find_map(|r| match r {
            Record::Observation(obs) => Some(obs),
            _ => None,
        })
        .expect("an observation")
}

#[test]
fn a_drive_that_gets_a_fix_partway_through_says_where_each_row_actually_was() {
    // The reason the source is a column and not a session-wide setting: a
    // capture that starts in a garage and ends on a road is one capture.
    let clock = Clock::new();
    let gps = Gps::detached();
    let config = EngineConfig {
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0))
            .with_gps(gps.clone(), DEFAULT_MAX_AGE),
        ..manual()
    };
    let mut engine = engine(config, &clock);

    let indoors = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:01", -60), clock.at(1));
    let indoors = observed(&indoors);
    assert_eq!(indoors.fix.source, PositionSource::Static);

    gps.feed(GGA, clock.at(10).unix_ms);
    let outdoors = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:02", -60), clock.at(11));
    let outdoors = observed(&outdoors);
    assert_eq!(outdoors.fix.source, PositionSource::Gps);
    assert_eq!(outdoors.fix.accuracy, Some(4.5));
    assert!((outdoors.fix.lat.expect("a latitude") - 48.1173).abs() < 1e-4);
}

#[test]
fn a_receiver_that_goes_quiet_hands_the_rows_back_to_the_static_position() {
    // Not "keeps the last fix forever": at driving speed a minute-old position
    // is a different street, and a row that claims it is worse than one that
    // admits to the operator's typed-in guess.
    let clock = Clock::new();
    let gps = Gps::detached();
    let config = EngineConfig {
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0))
            .with_gps(gps.clone(), DEFAULT_MAX_AGE),
        ..manual()
    };
    let mut engine = engine(config, &clock);
    gps.feed(GGA, clock.at(10).unix_ms);

    let fresh = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:03", -60), clock.at(12));
    let fresh = observed(&fresh);
    assert_eq!(fresh.fix.source, PositionSource::Gps);

    let stale = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:04", -60), clock.at(60));
    let stale = observed(&stale);
    assert_eq!(stale.fix.source, PositionSource::Static);
    assert_eq!(stale.fix.lat, Some(37.7749));

    // And the snapshot the operator is watching says the same thing, so the
    // header and the rows can never disagree about where the fleet thinks it is.
    let snapshot = engine.snapshot(clock.at(60), StoreStats::default());
    assert_eq!(snapshot.position.source, PositionSource::Static);
    assert!(snapshot.gps.is_some(), "the receiver is still configured, just not believed");
}

#[test]
fn raw_frames_are_only_kept_when_asked_for() {
    let clock = Clock::new();
    let mut off = engine(manual(), &clock);
    let mut on = engine(EngineConfig { record_raw: true, ..manual() }, &clock);

    let has_raw = |batch: &wartui_core::ActionBatch| {
        batch.records.iter().any(|r| matches!(r, Record::Raw(_)))
    };
    assert!(!has_raw(&off.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1))));
    assert!(has_raw(&on.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1))));
}

#[test]
fn nodes_are_ordered_by_mac_so_the_table_does_not_reshuffle() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(heartbeat(NODE, 1), clock.at(2));

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
    let mut engine = engine(manual(), &clock);
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
    let mut engine = engine(manual(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    // Nothing goes out at the moment the operator asks. A node's radio is away
    // scanning some other channel for all but the 300 ms after its own
    // heartbeat, so a frame sent now would be transmitted into silence.
    let asked = engine.handle(assign(NODE, 5, 5), clock.at(2));
    assert!(asked.urgent.is_empty(), "nothing is sent until the window opens");

    let opened = engine.handle(heartbeat(NODE, 2), clock.at(6));
    let (_, dst, admin) = sent_admin(&opened);
    assert_eq!(dst, NODE);
    assert_eq!(admin.channels, run(5, 5));
    assert_eq!(admin.epoch, 1, "the first epoch of a fresh database");
    assert_eq!(counters(&engine).admin_sent, 1);
}

#[test]
fn a_node_heard_only_through_its_observations_gets_nothing_yet() {
    // A node reports what it found on a channel before it gets back to the
    // control channel to heartbeat, so this is an ordinary few seconds in the
    // life of a node that is about to be perfectly drivable. Until the
    // heartbeat arrives nothing says which band its radio reaches, and a share
    // of 5 GHz cut for a C6 is a share nobody scans.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));

    let asked = engine.handle(assign(NODE, 0, 10), clock.at(2));
    assert!(asked.urgent.is_empty());
    assert_eq!(counters(&engine).admin_sent, 0);

    let node = engine.nodes().next().expect("it is still in the table");
    assert!(node.last_heartbeat.is_none(), "nothing has opened a window yet");
    assert!(node.desired.is_none(), "and nothing is queued against it");
}

#[test]
fn a_stranger_in_the_fleet_does_not_take_a_share_of_the_pool() {
    // The failure this exists to prevent, in the smallest form that shows it:
    // two nodes plus a stranger must partition the pool two ways, not three.
    // Three ways would leave a third of the pool assigned to a node that will
    // never scan it, so the fleet would cover less than the two would alone.
    //
    // It used to be the capability token that caught this, because a stock
    // node's heartbeat was byte-identical to ours. The magic catches it a
    // layer earlier now: the stranger never reaches the fleet table at all.
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(rx(THIRD, &[0x45, 0x4E, 0x4F, 0x57, 0x03, 0x93, 0x00, 0x00, 0x00]), clock.at(1));
    engine.handle(Event::Tick, clock.at(2));

    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    let plan = snapshot.plan.expect("a plan");
    assert_eq!(plan.node_count(), 2, "the stranger is not one of the two");
    assert_eq!(snapshot.alive, 2, "and is not in the table to be counted alive");
    assert_eq!(snapshot.assignable, 2);
    assert_eq!(counters(&engine).foreign_fleet, 1, "but it was heard, and it is said so");

    let held: Vec<ChannelSet> =
        engine.nodes().filter_map(|node| node.desired.map(|a| a.channels)).collect();
    assert_eq!(held.len(), 2, "only the two get channels");
    let union = held.iter().fold(ChannelSet::empty(), |mut acc, set| {
        for idx in set.indices() {
            acc.insert(idx);
        }
        acc
    });
    assert_eq!(union, ChannelPool::Us.channels(), "and between them they hold all of it");
}

#[test]
fn a_two_point_four_node_is_never_dealt_a_channel_it_cannot_tune() {
    // A C6 adopts a 5 GHz share and acknowledges it, then scans the part it can
    // reach — so the rest is a hole in the fleet's coverage with an assignment
    // sitting on top of it, and nothing on screen would say so. This is the
    // same failure the capability token prevents for a node that is not ours at
    // all, arriving through a node that is.
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(narrowband_heartbeat(OTHER, 1), clock.at(1));
    engine.handle(Event::Tick, clock.at(2));

    let held: Vec<(Mac, ChannelSet)> =
        engine.nodes().filter_map(|node| node.desired.map(|a| (node.mac, a.channels))).collect();
    assert_eq!(held.len(), 2, "both are ours and both are planned for");
    for (mac, channels) in &held {
        if *mac == OTHER {
            assert!(
                channels.indices().all(|idx| !wartui_proto::plan::is_five_ghz(idx)),
                "the 2.4 GHz node was given 5 GHz channels"
            );
        }
    }
    // And between them they still cover the pool, because the C5 takes what the
    // C6 cannot: the fleet is not made worse by the C6 being in it.
    let union = held.iter().fold(ChannelSet::empty(), |mut acc, (_, set)| {
        for idx in set.indices() {
            acc.insert(idx);
        }
        acc
    });
    assert_eq!(union, ChannelPool::Us.channels());
}

#[test]
fn the_bluetooth_scan_is_not_given_to_a_node_that_has_no_bluetooth_in_it() {
    // The `ble` cargo feature decides whether the scan code exists; the flag
    // decides whether it runs. A node built without the feature would adopt the
    // flag, acknowledge the frame and scan nothing, and `ble_node` — the only
    // record of who was asked — would name it for the rest of the capture.
    let clock = Clock::new();
    let mut known = engine(manual(), &clock);
    known.handle(narrowband_heartbeat(NODE, 1), clock.at(1));
    known.handle(Event::Command(Command::AssignBle { mac: Some(NODE) }), clock.at(2));
    assert_eq!(known.snapshot(clock.at(3), StoreStats::default()).ble_node, None);

    // Naming a node before it has been heard from is allowed — an operator or a
    // script may know its address first — and taken back on the tick that finds
    // out what it is, rather than being refused on a guess.
    let mut later = engine(manual(), &clock);
    later.handle(Event::Command(Command::AssignBle { mac: Some(NODE) }), clock.at(1));
    assert_eq!(later.snapshot(clock.at(2), StoreStats::default()).ble_node, Some(NODE));
    later.handle(narrowband_heartbeat(NODE, 1), clock.at(3));
    later.handle(Event::Tick, clock.at(4));
    assert_eq!(
        later.snapshot(clock.at(5), StoreStats::default()).ble_node,
        None,
        "taken back once the node said it cannot run one"
    );
}

#[test]
fn a_node_that_stops_claiming_bluetooth_is_told_to_stop_rather_than_merely_forgotten() {
    // Forgetting who held the scan is not the same as taking it off them. The
    // flag rides in the assignment frame, so a node still holding one goes on
    // being shown as a holder — the fleet table reads the flag off the
    // assignment, not off `ble_node` — and `b` could not clear it either,
    // because it asks `ble_node` whether the node holds anything and would
    // offer to turn Bluetooth *on*, only to be refused for a build without it.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(heartbeat(NODE, 5), clock.at(1));
    engine.handle(assign(NODE, 0, 10), clock.at(2));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(NODE) }), clock.at(3));
    let (id, _, admin) = sent_admin(&engine.handle(heartbeat(NODE, 6), clock.at(4)));
    assert!(admin.scan_ble(), "it holds the scan to begin with");
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(4));

    // The same board reflashed without the `ble` feature: a counter that went
    // backwards, and a token that no longer claims a scan.
    let batch = engine.handle(narrowband_heartbeat(NODE, 1), clock.at(10));
    let (id, _, admin) = sent_admin(&batch);
    assert!(!admin.scan_ble(), "the withdrawal rides in the frame the reboot was sending anyway");
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(10));

    engine.handle(Event::Tick, clock.at(11));
    assert_eq!(engine.snapshot(clock.at(11), StoreStats::default()).ble_node, None);
    let node = engine.nodes().next().expect("the node");
    assert!(!node.confirmed.expect("still assigned").ble, "and the node is not still holding it");
    assert!(!node.dirty, "one epoch was enough; the tick does not spend a second");
}

#[test]
fn a_hand_assignment_is_cut_down_to_what_the_node_can_actually_tune() {
    // `A` offers the whole pool, and the planner is not involved in a fleet
    // driven by hand — so without this the exact failure the capability token
    // exists to prevent is one keypress away, and a quieter one: nothing
    // re-partitions afterwards to correct it and `Plan::unreachable` never sees
    // a hand assignment.
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(narrowband_heartbeat(NODE, 1), clock.at(1));
    let whole_pool =
        Event::Command(Command::Assign { mac: NODE, channels: ChannelPool::Us.channels() });
    engine.handle(whole_pool, clock.at(2));

    let (id, _, admin) = sent_admin(&engine.handle(narrowband_heartbeat(NODE, 2), clock.at(6)));
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(6));
    assert_eq!(admin.channels, run(0, 10), "2.4 GHz only, and nothing said it would be more");
    for idx in admin.channels.indices() {
        assert!(!is_five_ghz(idx), "index {idx} needs a radio this node does not have");
    }

    // And a set with nothing in it the node can reach is no assignment at all,
    // which is the empty-set rule arriving by a different road.
    let mut five = ChannelSet::empty();
    five.insert(FIRST_FIVE_GHZ_INDEX);
    engine.handle(Event::Command(Command::Assign { mac: NODE, channels: five }), clock.at(7));
    assert!(engine.handle(narrowband_heartbeat(NODE, 3), clock.at(11)).urgent.is_empty());
    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.confirmed.or(node.desired).expect("still assigned").channels, run(0, 10));
}

#[test]
fn an_empty_set_is_not_an_assignment_and_never_reaches_a_node() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    // There is no frame meaning "scan nothing". A node that adopted one would
    // park on the control channel and collect nothing, while the host had a
    // MAC-layer acknowledgement for it and so read the row as confirmed at
    // `0: none` — a node doing nothing that looks exactly like a node doing as
    // it was told. `replan` already skips a node it has nothing for.
    let empty = Event::Command(Command::Assign { mac: NODE, channels: ChannelSet::empty() });
    engine.handle(empty, clock.at(2));
    assert!(engine.handle(heartbeat(NODE, 2), clock.at(6)).urgent.is_empty());
    assert_eq!(counters(&engine).admin_sent, 0);

    // And it does not take away what a node already holds, which is the shape
    // this would arrive in: a set narrowed one channel at a time until there is
    // nothing left of it.
    engine.handle(assign(NODE, 0, 10), clock.at(7));
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(11)));
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(11));

    let empty = Event::Command(Command::Assign { mac: NODE, channels: ChannelSet::empty() });
    engine.handle(empty, clock.at(12));
    assert!(engine.handle(heartbeat(NODE, 4), clock.at(16)).urgent.is_empty());
    let node = engine.nodes().next().expect("the node");
    assert!(!node.dirty, "nothing was asked for, so there is nothing pending");
    assert_eq!(node.confirmed.expect("still assigned").channels, run(0, 10));
}

#[test]
fn an_assignment_is_believed_only_once_the_node_radio_acknowledges_it() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 0, 10), clock.at(2));
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // Divergence 3. The vendor core clears its dirty flag from the
    // `esp_now_send` return value, so an enqueue that nothing received still
    // counts as an assignment delivered.
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(6));
    let node = engine.nodes().next().expect("the node");
    assert!(!node.dirty, "acknowledged, so there is nothing left to deliver");
    assert_eq!(node.confirmed.expect("confirmed").channels, run(0, 10));
    assert_eq!(counters(&engine).admin_acked, 1);

    let Some(Record::Assignment(row)) = batch.records.first() else {
        panic!("every attempt is written down")
    };
    assert_eq!(row.outcome, AdminOutcome::Acked);
    assert_eq!(row.channels, run(0, 10));
    assert!(!row.ble);

    // And a later heartbeat sends nothing, because there is nothing to send.
    assert!(engine.handle(heartbeat(NODE, 3), clock.at(10)).urgent.is_empty());
}

#[test]
fn an_unacknowledged_assignment_is_retried_on_the_next_heartbeat() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
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
        retry.epoch, first.epoch,
        "a retry of the same assignment keeps its epoch; the node never saw it"
    );
    assert_eq!(counters(&engine).admin_failed, 1);
}

#[test]
fn an_assignment_the_bridge_never_answers_for_is_written_down_as_unknown() {
    let clock = Clock::new();
    let config = EngineConfig { admin_timeout: Duration::from_secs(2), ..manual() };
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
    let config = EngineConfig { admin_timeout: Duration::from_secs(2), ..manual() };
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
    assert_eq!(node.confirmed.expect("confirmed").channels, run(5, 5));

    // Now the stranded first attempt times out. It earns its row — that attempt
    // really did go unanswered — but the node is demonstrably holding the
    // range, and the view must not start claiming otherwise.
    let batch = engine.handle(Event::Tick, clock.at(5));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Silent);

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.last_outcome, Some(AdminOutcome::Acked));
    assert_eq!(node.confirmed.expect("still confirmed").channels, run(5, 5));
    assert!(!node.dirty, "nothing is owed: the node acknowledged this epoch");
}

#[test]
fn the_assignment_latency_is_measured_end_to_end_on_the_bridge_clock() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 7, 7), clock.at(2));

    // The heartbeat is stamped by the bridge at 1_000_000 µs of its own uptime
    // and the transmit callback at 1_004_500: 4.5 ms from "the node is
    // listening" to "its radio says it has the frame", with none of the host's
    // scheduling in between.
    let opened =
        engine.handle(beat_at(NODE, 2, Capabilities::here(true, true), 1_000_000), clock.at(6));
    let (id, _, _) = sent_admin(&opened);
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 1_004_500), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, Some(4_500));
    assert_eq!(engine.nodes().next().expect("the node").last_latency_us, Some(4_500));
}

#[test]
fn a_latency_measured_across_the_bridge_counter_wrapping_is_still_right() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    // Both heartbeats are stamped just short of the wrap, a millisecond apart,
    // which is what a bridge approaching 71 minutes of uptime actually emits.
    engine.handle(beat_at(NODE, 1, Capabilities::here(true, true), u32::MAX - 2_000), clock.at(1));
    engine.handle(assign(NODE, 7, 7), clock.at(2));

    // The bridge's stamp is a `u32` of microseconds and so wraps every 71
    // minutes or so. A heartbeat 1 ms before the wrap and a callback 3.5 ms
    // after it are 4.5 ms apart, and a wrapping subtraction says so.
    let opened = engine
        .handle(beat_at(NODE, 2, Capabilities::here(true, true), u32::MAX - 999), clock.at(6));
    let (id, _, _) = sent_admin(&opened);
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 3_500), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, Some(4_500));
}

#[test]
fn a_reboot_re_issues_the_assignment_under_a_fresh_epoch() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
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
    assert_ne!(reissued.epoch, first.epoch);
    assert_eq!(reissued.channels, run(2, 4));

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
    let config = EngineConfig { assignment_base: 300, ..manual() };
    let mut engine = engine(config, &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(assign(NODE, 1, 1), clock.at(2));

    let (_, _, admin) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    // 301 narrowed to a byte that skips zero, which the firmware never sends.
    assert_eq!(admin.epoch, 46);
}

#[test]
fn the_fleet_arithmetic_travels_with_the_assignment_rather_than_being_read_at_send_time() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
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
    let mut engine = engine(manual(), &clock);

    let batch = engine.handle(assign(NODE, 0, 0), clock.at(1));

    assert!(batch.urgent.is_empty());
    assert_eq!(engine.nodes().count(), 0, "asking about a node does not invent one");
}

#[test]
fn the_heartbeat_period_is_the_median_of_recent_sweeps() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);

    // Four-second sweeps, which is about what a node holding the whole US pool
    // does, with one heartbeat lost in the middle. The median is what keeps
    // that lost beat from reading as an assignment twice the size.
    for (n, at) in [1u64, 5, 9, 17, 21, 25].into_iter().enumerate() {
        engine.handle(heartbeat(NODE, n as u32 + 1), clock.at(at));
    }

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.beat_period_ms(), Some(4_000));
}

#[test]
fn a_bridge_that_cannot_transmit_at_all_is_a_failure_not_a_delivery() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
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
    let mut engine = engine(manual(), &clock);
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
    let mut engine = engine(manual(), &clock);
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
    assert_eq!(node.desired.expect("desired").channels, run(5, 9));
}

// ---------------------------------------------------------------------------
// Auto-assignment: the engine holding the whole fleet on a partition of the
// pool, which is what "replaces the core" means rather than "watches it".
// ---------------------------------------------------------------------------

/// The `n`th node of a fake fleet. Ordered by MAC, which is the order the
/// planner numbers them in.
fn peer(n: u8) -> Mac {
    [0x02, 0x00, 0x5E, 0x10, 0x57, n]
}

fn auto() -> EngineConfig {
    EngineConfig { auto: true, ..Default::default() }
}

/// The planner switched off, which is what every test here but one wants: with
/// it on, the plan is a second author of assignments and of the rows that
/// resolve them, so a test about anything else would be asserting against both
/// at once. What the default actually is belongs in one test, not in all of
/// them.
fn manual() -> EngineConfig {
    EngineConfig { auto: false, ..Default::default() }
}

/// Every index a collection of channel sets covers, sorted. Comparing this
/// against the pool's own indices proves coverage and disjointness at once: a
/// gap makes it short, an overlap makes it long, and a channel from outside the
/// pool puts an index in it that the pool does not have.
fn covered(sets: &[ChannelSet]) -> Vec<u8> {
    let mut all: Vec<u8> = sets.iter().flat_map(|set| set.indices()).collect();
    all.sort_unstable();
    all
}

fn pool_indices(pool: ChannelPool) -> Vec<u8> {
    pool.runs().iter().flat_map(|r| r.start..=r.end).collect()
}

/// What each node has been told to scan, whether or not it has answered yet.
fn wanted(engine: &FleetEngine) -> Vec<ChannelSet> {
    engine.nodes().filter_map(|node| node.desired).map(|a| a.channels).collect()
}

#[test]
fn auto_assignment_deals_the_whole_pool_out_across_the_fleet() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }

    // Covering the pool exactly is coverage and disjointness in one assertion.
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));

    // And every node holds some of both bands, which is what dealing buys over
    // block-splitting: one node dropping out thins the fleet's coverage rather
    // than blinding it to 2.4 GHz until the next re-cut lands.
    for node in engine.nodes() {
        let set = node.desired.expect("every member of the plan has channels").channels;
        for band in ChannelPool::Us.runs() {
            assert!(
                (band.start..=band.end).any(|idx| set.contains(idx)),
                "node got nothing from {band:?}"
            );
        }
    }

    // `node_index` and `node_count` drive the transmit stagger
    // (`src/RadioTuning.cpp:3-13`), so they have to agree fleet-wide: unique
    // indices over one shared count, not a numbering restarted per run.
    let indices: Vec<(u8, u8)> = engine
        .nodes()
        .map(|node| {
            let a = node.desired.expect("every member of the plan has channels");
            (a.node_index, a.node_count)
        })
        .collect();
    assert_eq!(indices, vec![(0, 3), (1, 3), (2, 3)]);
}

#[test]
fn a_fleet_is_partitioned_without_being_asked_and_stops_when_told() {
    // The default, and the difference between wartui and a monitor: a fleet
    // nobody has partitioned is a fleet of nodes all sweeping the same
    // channels, which is the thing this whole program exists to stop. The one
    // test in this file built on `EngineConfig::default()`, so that the day
    // the default changes it is this that fails rather than everything else.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    for n in 0..3 {
        let batch = engine.handle(heartbeat(peer(n), 1), clock.at(1));
        assert!(!batch.urgent.is_empty(), "a node's share goes out in its own window");
    }
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));

    // Switching it off hands the fleet back: what is already out there stays
    // out there, and the change that would have re-cut the pool a moment ago —
    // a node joining — now goes unanswered.
    engine.handle(Event::Command(Command::SetAuto(false)), clock.at(2));
    engine.handle(heartbeat(peer(3), 1), clock.at(3));
    let joined = engine.nodes().find(|node| node.mac == peer(3)).expect("the new node");
    assert!(joined.desired.is_none(), "the planner has stopped cutting");
    assert!(engine.snapshot(clock.at(3), StoreStats::default()).plan.is_none());

    // And switching it back on partitions what is already there rather than
    // waiting for the fleet to change shape first.
    engine.handle(Event::Command(Command::SetAuto(true)), clock.at(4));
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));
}

#[test]
fn a_node_joining_re_cuts_the_pool_for_the_whole_fleet() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    // A node's own first heartbeat is what admits it, and the window it has
    // just opened is the one its share goes out in — not the next one.
    let (id, dst, first) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    assert_eq!(dst, peer(0));
    assert_eq!(first.node_count, 1);
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(1));

    let (_, _, second) = sent_admin(&engine.handle(heartbeat(peer(1), 1), clock.at(2)));
    assert_eq!(second.node_count, 2, "the fleet the range was cut for");

    // The node that was already settled is owed a new range as well: its own
    // stagger slot is computed from a count that has just changed, and a fleet
    // whose members disagree about the count keys up on top of itself.
    let settled = engine.nodes().next().expect("the first node");
    assert!(settled.dirty, "re-issued, though it was acknowledged a moment ago");
    assert_eq!(settled.desired.expect("a new range").node_count, 2);
    assert_eq!(settled.confirmed.expect("what it still holds").node_count, 1);

    let (_, dst, third) = sent_admin(&engine.handle(heartbeat(peer(0), 2), clock.at(6)));
    assert_eq!(dst, peer(0), "and it goes out in that node's own next window");
    assert_ne!(third.epoch, first.epoch, "under a fresh epoch");
    assert_eq!(counters(&engine).replans, 2);
}

#[test]
fn a_node_that_stops_heartbeating_leaves_the_plan_and_the_rest_take_its_channels() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(heartbeat(peer(1), 1), clock.at(2));
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));

    // Divergence 2: topology is driven by heartbeats alone. A node that is not
    // heartbeating never opens an admin window, so a range held open for it is
    // a share of the pool nobody is scanning.
    engine.handle(heartbeat(peer(1), 2), clock.at(70));

    let departed = engine.nodes().next().expect("the node that went quiet");
    assert!(departed.desired.is_none(), "nothing is owed to a node that cannot receive it");
    assert!(!departed.dirty);
    let survivor = engine.nodes().nth(1).expect("the node still beating");
    assert_eq!(survivor.desired.expect("a range").node_count, 1);
}

#[test]
fn one_node_on_a_two_run_pool_gets_all_of_it_in_one_frame() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);

    // This is where the rotation used to be. An assignment carried one contiguous
    // range and the US pool is two runs, so a lone node was given them in turn
    // on a sixty-second dwell: half the pool unscanned at any instant, and a
    // fresh epoch spent every minute for as long as the fleet stayed at one.
    // A channel mask says both runs at once.
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    assert_eq!(first.channels, ChannelPool::Us.channels());
    assert_eq!(first.channels.len(), 34, "eleven 2.4 GHz channels and twenty-three 5 GHz");
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(1));

    // And nothing re-issues it. Ticks are what the dwell timer used to fire on,
    // so a plan that is still holding after several minutes of them is the
    // whole of the change.
    for minute in 1..=5 {
        let batch = engine.handle(Event::Tick, clock.at(60 * minute));
        assert!(batch.urgent.is_empty(), "nothing is owed at minute {minute}");
    }
    assert_eq!(counters(&engine).replans, 1, "one plan, once");
}

#[test]
fn bluetooth_goes_to_one_node_at_a_time_and_moving_it_tells_both() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    // Both join, then both settle: the second node's arrival re-cuts the pool,
    // so the shares that stick are the ones sent on the second heartbeat.
    for n in 0..2 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    for n in 0..2 {
        let (id, ..) = sent_admin(&engine.handle(heartbeat(peer(n), 2), clock.at(2)));
        engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(2));
    }
    // Nobody scans Bluetooth unless asked. The cost is measured and real —
    // ~10% of the sweep period on our own firmware, and every assignment lost
    // on the vendor's — so it is not a fleet-wide default.
    assert!(engine.nodes().all(|node| !node.confirmed.expect("planned").ble));
    assert_eq!(engine.snapshot(clock.at(2), StoreStats::default()).ble_node, None);

    let held = engine.nodes().next().expect("the node").confirmed.expect("planned").channels;
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(3));
    let (id, dst, admin) = sent_admin(&engine.handle(heartbeat(peer(0), 3), clock.at(4)));
    assert_eq!(dst, peer(0));
    assert!(admin.scan_ble(), "the flag rides on that node's own next assignment");
    assert_eq!(admin.channels, held, "and changes nothing else about the assignment");
    assert_ne!(admin.epoch, 0, "under an epoch the node will adopt");
    engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(4));

    // Moving it is two frames: the node giving it up has to be told as well,
    // or the fleet would briefly have two nodes holding the shared antenna.
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(1)) }), clock.at(5));
    let (_, _, gave_up) = sent_admin(&engine.handle(heartbeat(peer(0), 4), clock.at(6)));
    assert!(!gave_up.scan_ble());
    let (_, _, took_it) = sent_admin(&engine.handle(heartbeat(peer(1), 3), clock.at(7)));
    assert!(took_it.scan_ble());
}

#[test]
fn the_bluetooth_assignment_does_not_outlive_the_node_holding_it() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(heartbeat(peer(1), 1), clock.at(1));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(2));
    assert_eq!(engine.snapshot(clock.at(2), StoreStats::default()).ble_node, Some(peer(0)));

    // It ages out with the node. Leaving it named would have the view claiming
    // the fleet has Bluetooth coverage that nothing is providing — and the
    // flag would silently come back if that MAC ever reappeared.
    engine.handle(heartbeat(peer(1), 2), clock.at(70));
    engine.handle(Event::Tick, clock.at(70));
    assert_eq!(engine.snapshot(clock.at(70), StoreStats::default()).ble_node, None);
}

#[test]
fn a_settled_fleet_is_left_alone_rather_than_re_issued_every_tick() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    // Acknowledge each node's range, which takes the fleet to the state it
    // spends the whole capture in.
    for n in 0..3 {
        let (id, _, _) = sent_admin(&engine.handle(heartbeat(peer(n), 2), clock.at(5)));
        engine.handle(send_result(id, SendStatus::AckOk, 900), clock.at(5));
    }
    let settled = counters(&engine).admin_sent;

    // A node adopts on `!=`, so an epoch it already holds is a frame it
    // acknowledges and discards. Re-cutting an unchanged fleet would spend the
    // rest of the capture doing exactly that at heartbeat rate.
    for beat in 3..6u32 {
        let at = clock.at(u64::from(10 + beat));
        engine.handle(Event::Tick, at);
        for n in 0..3 {
            let batch = engine.handle(heartbeat(peer(n), beat), at);
            assert!(batch.urgent.is_empty(), "nothing left to say");
        }
    }
    assert_eq!(counters(&engine).admin_sent, settled);
    assert_eq!(counters(&engine).replans, 3, "once per node that joined, and no more");
}

#[test]
fn a_fleet_larger_than_the_radio_can_address_stops_being_re_partitioned() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    for n in 0..=u8::try_from(wartui_proto::plan::MAX_NODES).expect("twenty fits") {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }

    // Twenty peers is the radio's whole table, so the twenty-first node is one
    // the bridge cannot address. Partitioning the pool across it anyway would
    // hand the other twenty ranges cut for a fleet that includes a node which
    // never hears the result.
    let last = engine.nodes().last().expect("the twenty-first node");
    assert!(last.desired.is_none(), "the node past the limit is given nothing");
    assert_eq!(
        engine.nodes().filter(|node| node.desired.is_some()).count(),
        wartui_proto::plan::MAX_NODES,
        "and the twenty already placed keep the ranges they were given"
    );
}

#[test]
fn taking_the_fleet_back_by_hand_numbers_it_the_way_the_plan_did() {
    // The path this is on: the planner runs by default, so `p` then `a` is how
    // anyone assigns anything. If the hand-assigned node counted the fleet
    // differently from the plan the others are still holding, it would compute
    // its transmit stagger against a fleet size nobody else agrees with — and
    // a node that went quiet an hour ago would be enough to cause it.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    // A fourth node, heard from once long enough ago to have aged out of the
    // plan. It still has a row, and it sorts before the others by MAC.
    engine.handle(heartbeat(GONE, 1), clock.at(1));
    engine.handle(heartbeat(peer(0), 2), clock.at(120));
    engine.handle(heartbeat(peer(1), 2), clock.at(120));
    engine.handle(heartbeat(peer(2), 2), clock.at(120));
    let planned = engine
        .nodes()
        .find(|node| node.mac == peer(1))
        .and_then(|node| node.desired)
        .expect("the plan gave it channels");

    engine.handle(Event::Command(Command::SetAuto(false)), clock.at(121));
    engine.handle(assign(peer(1), 4, 4), clock.at(122));
    let by_hand = engine
        .nodes()
        .find(|node| node.mac == peer(1))
        .and_then(|node| node.desired)
        .expect("the hand assignment");

    assert_eq!(by_hand.channels, run(4, 4), "the channels are the operator\'s");
    assert_eq!(
        (by_hand.node_index, by_hand.node_count),
        (planned.node_index, planned.node_count),
        "but the stagger slot is the fleet\'s"
    );
}

#[test]
fn channels_given_by_hand_keep_the_stagger_slot_the_plan_gave_the_node() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }

    // An operator narrowing one node is changing what it scans, not where in
    // the 120 ms stagger window it keys up. Renumbering it would put it on top
    // of another node's slot for as long as the override lasted.
    engine.handle(assign(peer(2), 20, 22), clock.at(2));
    let node = engine.nodes().nth(2).expect("the third node");
    let desired = node.desired.expect("the hand-given channels");
    assert_eq!(desired.channels, run(20, 22));
    assert_eq!((desired.node_index, desired.node_count), (2, 3));
}

#[test]
fn a_node_that_rejoins_holding_the_right_channels_is_still_re_issued_when_it_reboots() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    let (first, _, _) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    engine.handle(send_result(first, SendStatus::AckOk, 900), clock.at(1));
    let (second, _, _) = sent_admin(&engine.handle(heartbeat(peer(1), 1), clock.at(2)));
    engine.handle(send_result(second, SendStatus::AckOk, 900), clock.at(2));
    let (recut, _, _) = sent_admin(&engine.handle(heartbeat(peer(0), 2), clock.at(3)));
    engine.handle(send_result(recut, SendStatus::AckOk, 900), clock.at(3));

    // It goes quiet long enough to leave the plan, then comes back — and the
    // re-cut gives it the very channels it is already holding, so there is
    // nothing to send it.
    engine.handle(heartbeat(peer(1), 2), clock.at(70));
    engine.handle(heartbeat(peer(0), 3), clock.at(71));
    assert!(
        engine.handle(heartbeat(peer(0), 4), clock.at(75)).urgent.is_empty(),
        "a node already scanning its share is not told so again"
    );

    // A reboot forgets the range and the node's own version byte with it, so
    // its share has to be said again under a fresh epoch. Saying it needs
    // wartui to still know what the node was holding — and a node that rejoined
    // a plan without being sent anything is exactly the case where it might
    // not. Getting this wrong leaves the node parked on the control channel
    // collecting nothing, which is silent rather than merely wrong.
    let batch = engine.handle(heartbeat(peer(0), 1), clock.at(80));
    let (_, dst, admin) = sent_admin(&batch);
    assert_eq!(dst, peer(0));
    assert_eq!(
        admin.channels,
        plan(ChannelPool::Us, 2).expect("two nodes").channels_for(0).expect("assigned")
    );
    assert_eq!(admin.node_count, 2);
}

#[test]
fn a_node_the_bridge_cannot_peer_with_leaves_the_plan_so_the_rest_still_cover_the_pool() {
    let clock = Clock::new();
    let mut engine = engine(auto(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    let (id, dst, _) = sent_admin(&engine.handle(heartbeat(peer(2), 2), clock.at(5)));
    assert_eq!(dst, peer(2));

    // Peers are never removed, so a session that has churned through twenty
    // nodes fills the table even though far fewer are alive at once. A node the
    // bridge cannot address is no use to a partition: its share would go
    // unscanned for the rest of the capture, and the pool would quietly not be
    // covered while the view said it was.
    engine.handle(send_result(id, SendStatus::PeerTableFull, 900), clock.at(5));
    engine.handle(Event::Tick, clock.at(6));

    assert!(engine.nodes().nth(2).expect("the refused node").desired.is_none());
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));
    assert_eq!(
        engine.nodes().filter(|node| node.desired.is_some()).count(),
        2,
        "the two the bridge can reach hold the whole pool between them"
    );

    // A bridge announcing itself has an empty peer table, so the node is worth
    // trying again.
    engine.handle(connected(), clock.at(7));
    engine.handle(Event::Tick, clock.at(8));
    assert!(engine.nodes().nth(2).expect("the refused node").desired.is_some());
}

/// The bridge holds what it hears while no host is attached, so the first
/// thing a fresh connection receives is a ring's worth of the recent past
/// delivered as fast as USB will carry it. Measured on the bench behind
/// `docs/phase-4-findings.md`: twenty-five frames spanning eight and a half
/// minutes of bridge time inside seventeen milliseconds of host time.
///
/// Every one of those heartbeats names an admin window that shut long ago.
/// Transmitting into them put nine assignments on the control channel in a
/// quarter of a second, eight of them unacknowledged, aimed at a node that was
/// off sweeping a channel where it could not hear any of them.
#[test]
fn heartbeats_replayed_out_of_the_bridges_backlog_do_not_open_admin_windows() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(connected(), clock.at_ms(0));

    // Nine heartbeats a second apart on the bridge's clock, handed to the
    // engine two milliseconds apart on the host's. Only one of those can be
    // real time.
    let mut sent = 0;
    for beat in 0..9u32 {
        let batch = engine.handle(
            beat_at(NODE, beat + 1, Capabilities::here(true, true), 1_000_000 + beat * 1_000_000),
            clock.at_ms(u64::from(beat) * 2),
        );
        sent += batch.urgent.len();
        // Owed from the first heartbeat on, so every window after this one is
        // a window the engine had something to say in and declined to.
        if beat == 0 {
            engine.handle(assign(NODE, 7, 7), clock.at_ms(1));
            assert!(engine.nodes().next().expect("the node").dirty, "an assignment is owed");
        }
    }

    assert_eq!(sent, 0, "not one assignment fired into a window that had already closed");
    assert_eq!(
        engine.counters().admin_windows_missed,
        8,
        "eight of the nine: the first arrived before the assignment was made, and a window \
         that owed nothing is not one this host held anything back from"
    );
    assert!(engine.nodes().next().expect("the node").dirty, "still owed, and still not sent");
    assert_eq!(engine.counters().admin_sent, 0);
    // The node is admitted regardless: a stale heartbeat still says it was
    // alive and what its radio is, and both are still true.
    assert_eq!(engine.nodes().count(), 1);
}

/// The other half of the same rule: once the host is reading the link live,
/// every heartbeat opens a window again. Without this the fix would be a way
/// of never assigning anything.
#[test]
fn a_heartbeat_heard_live_opens_an_admin_window_again() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(connected(), clock.at_ms(0));

    // A backlog, then the host catches up: it waits a full second of its own
    // clock for a frame the bridge stamped a second later, which is what being
    // caught up looks like from here.
    for beat in 0..4u32 {
        engine.handle(
            beat_at(NODE, beat + 1, Capabilities::here(true, true), 1_000_000 + beat * 1_000_000),
            clock.at_ms(u64::from(beat) * 2),
        );
    }
    engine.handle(assign(NODE, 7, 7), clock.at_ms(10));

    let live = engine
        .handle(beat_at(NODE, 5, Capabilities::here(true, true), 5_000_000), clock.at_ms(1_010));
    let (_, dst, _) = sent_admin(&live);
    assert_eq!(dst, NODE, "the window this heartbeat opened is the live one");
}

/// A latency is a measurement of one specific thing — how long an assignment
/// took to land inside the window a heartbeat opened — and a figure larger
/// than the window is not that measurement. The bench recorded 80 seconds this
/// way, off a heartbeat replayed out of the backlog: real arithmetic on two
/// honest stamps, saying nothing about how quickly the bridge answered.
#[test]
fn a_latency_longer_than_the_admin_window_is_not_recorded_as_one() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(beat_at(NODE, 1, Capabilities::here(true, true), 1_000_000), clock.at(1));
    engine.handle(assign(NODE, 7, 7), clock.at(2));

    let opened =
        engine.handle(beat_at(NODE, 2, Capabilities::here(true, true), 2_000_000), clock.at(6));
    let (id, _, _) = sent_admin(&opened);
    // The callback comes back a full second after the heartbeat, which is more
    // than three times the window the node was holding open.
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 3_000_000), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, None, "no number is better than a wrong one");
    assert_eq!(row.outcome, AdminOutcome::Acked, "the outcome is still recorded");
    assert_eq!(engine.nodes().next().expect("the node").last_latency_us, None);
}

/// A latency exactly at the window is still a latency: the node was listening
/// for that whole 300 ms, so the boundary belongs inside.
#[test]
fn a_latency_at_the_edge_of_the_admin_window_is_still_recorded() {
    let clock = Clock::new();
    let mut engine = engine(manual(), &clock);
    engine.handle(beat_at(NODE, 1, Capabilities::here(true, true), 1_000_000), clock.at(1));
    engine.handle(assign(NODE, 7, 7), clock.at(2));

    let opened =
        engine.handle(beat_at(NODE, 2, Capabilities::here(true, true), 2_000_000), clock.at(6));
    let (id, _, _) = sent_admin(&opened);
    let batch = engine.handle(send_result(id, SendStatus::AckOk, 2_300_000), clock.at(6));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, Some(300_000));
}
