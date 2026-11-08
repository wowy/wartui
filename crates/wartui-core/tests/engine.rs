//! The engine's rules, checked against a clock the test invents.
//!
//! Every one of these is a rule that would otherwise only be observable by
//! watching real hardware for a minute or more. Asserting them in microseconds
//! is the whole reason the engine reads no clock of its own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use indexmap::IndexSet;
use wartui_bridge::sim::minor_behind;
use wartui_bridge::{BridgeInfo, LinkEvent};
use wartui_core::ActionBatch;
use wartui_core::engine::{
    Command, Counters, EngineConfig, Event, FleetEngine, HostSample, NodeView, Now, StorePeaks,
    StoreStats,
};
use wartui_core::gps::{Gps, GpsConfig, GpsStatus};
use wartui_core::health::Health;
use wartui_core::position::{DEFAULT_MAX_AGE, PositionChain, PositionSource};
use wartui_core::record::{AdminOutcome, BatchGap, BridgeStatusSeen, HostStatus, Record};
use wartui_core::ubx::valset_rate;
use wartui_proto::bus::{
    Assign, Board, Capabilities, DieTemp, DownType, Heartbeat as HeartbeatMsg, MAX_SLOTS,
    NODE_VERSION, NodeBoard, RecordKind, Security, SightingBatchWriter, SightingMsg, Slot, UpType,
    UpdateState, UpdateStatus, Vdd, Version, wire_epoch,
};
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::link::{
    BridgeToHost, FromNodeBody, GPS_BUS_REFUSED, GPS_RECEIVER_ANSWERED, HostToBridge, LinkError,
    LogStr, LoopPhase, NO_GPS_RECEIVER, PowerOutcome, QueueId, QueueStat, QueueStats,
    RadioLinkStats, Undelivered,
};
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, ChannelSet, plan};
use wartui_proto::reset::ResetCause;

const NODE: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
const OTHER: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x85];
const THIRD: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x86];
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

/// An engine whose bridge has reported the default pool, as a run's first `Status` does.
fn engine(config: EngineConfig, clock: &Clock) -> FleetEngine {
    engine_on(ChannelPool::default(), config, clock)
}

/// An engine whose bridge has reported `pool`.
///
/// The default pool is `All`; a test that asserts the shape of a pool names the
/// one it means, so its arithmetic does not move when the default does.
fn engine_on(pool: ChannelPool, config: EngineConfig, clock: &Clock) -> FleetEngine {
    let mut engine = FleetEngine::new(config, clock.at(0));
    engine.handle(pool_status(pool), clock.at(0));
    engine
}

/// Every slot on the bus, as a `Status` from a full carrier reports it.
const ALL_PRESENT: u32 = (1 << MAX_SLOTS) - 1;

/// A `Status` with every counter zero and every slot present, reporting `pool`: where the planner
/// learns its pool.
fn pool_status(pool: ChannelPool) -> Event {
    slots_status(pool, ALL_PRESENT)
}

/// [`pool_status`] showing only the slots in `present`.
fn slots_status(pool: ChannelPool, present: u32) -> Event {
    Event::Link(LinkEvent::Message(BridgeToHost::Status {
        rx_count: 0,
        dropped_tx: 0,
        uptime_ms: 0,
        rx_oversize: 0,
        queues: QueueStats::new(),
        radio_link: RadioLinkStats::default(),
        host_uart_errors: 0,
        fan: None,
        pool,
        present,
    }))
}

/// Every slot but `mac`'s present.
fn without(mac: Mac) -> u32 {
    ALL_PRESENT & !slot_of(mac).bit()
}

thread_local! {
    /// Which slot each node a test names sits in. Each test runs on a thread of its
    /// own, so each starts with an empty bus.
    static SEATS: RefCell<HashMap<Mac, Slot>> = RefCell::new(HashMap::new());
}

/// The slot `mac` sits in: the next free one the first time a test names it.
fn slot_of(mac: Mac) -> Slot {
    SEATS.with_borrow_mut(|seats| {
        let next = u8::try_from(seats.len()).expect("a handful of nodes");
        *seats.entry(mac).or_insert_with(|| {
            Slot::from_u8(next).unwrap_or_else(|| panic!("more than {MAX_SLOTS} nodes in a test"))
        })
    })
}

/// The node a test seated in `slot`.
fn seated(slot: Slot) -> Mac {
    SEATS.with_borrow(|seats| {
        seats.iter().find(|(_, s)| **s == slot).map(|(mac, _)| *mac).expect("a seated node")
    })
}

/// A record of type `kind` from `slot`, stamped `rx_us` by the bridge.
fn from_slot(slot: Slot, kind: u8, body: &[u8], rx_us: u32) -> Event {
    Event::Link(LinkEvent::Message(BridgeToHost::FromNode {
        slot,
        rx_us,
        kind,
        body: FromNodeBody::from_slice(body).expect("fits"),
    }))
}

/// A record from `src`'s slot.
fn rx(src: Mac, kind: UpType, body: &[u8]) -> Event {
    rx_at(src, kind, body, 0)
}

/// A record carrying the bridge's own microsecond stamp, which is what tells a
/// host reading live from one draining a backlog.
fn rx_at(src: Mac, kind: UpType, body: &[u8], rx_us: u32) -> Event {
    from_slot(slot_of(src), kind.as_u8(), body, rx_us)
}

/// A sighting batch from `src`'s slot.
fn sightings(src: Mac, body: &[u8]) -> Event {
    sightings_at(src, body, 0)
}

fn sightings_at(src: Mac, body: &[u8], rx_us: u32) -> Event {
    rx_at(src, UpType::Sightings, body, rx_us)
}

/// A heartbeat body from `src`: epoch 0, nothing refused, a power-on boot and no
/// readings. Tests change the field they are about.
fn beat_msg(src: Mac, counter: u32) -> HeartbeatMsg {
    HeartbeatMsg {
        capabilities: Capabilities::here(Board::Esp32c5),
        mac: src,
        counter,
        epoch: 0,
        wifi_refused: 0,
        ble_refused: 0,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
        heap_free: 0,
    }
}

/// `msg` arriving from its node's slot, stamped `rx_us`.
fn beat(msg: HeartbeatMsg, rx_us: u32) -> Event {
    rx_at(msg.mac, UpType::Heartbeat, &msg.encode(), rx_us)
}

/// A heartbeat from a node. Almost every test below wants this one.
/// Reports epoch 0: nothing adopted, which is what every test not about
/// adoption wants said.
fn heartbeat(src: Mac, counter: u32) -> Event {
    beat(beat_msg(src, counter), 0)
}

/// A heartbeat reporting the epoch the node holds, for the tests about
/// adoption.
fn heartbeat_holding(src: Mac, counter: u32, epoch: u8) -> Event {
    beat(HeartbeatMsg { epoch, ..beat_msg(src, counter) }, 0)
}

fn beat_at(src: Mac, counter: u32, epoch: u8, capabilities: Capabilities, rx_us: u32) -> Event {
    beat(HeartbeatMsg { epoch, capabilities, ..beat_msg(src, counter) }, rx_us)
}

/// A heartbeat carrying the node's since-boot pending-buffer refusals, Wi-Fi then BLE.
fn heartbeat_refusing(src: Mac, counter: u32, wifi_refused: u16, ble_refused: u16) -> Event {
    beat(HeartbeatMsg { wifi_refused, ble_refused, ..beat_msg(src, counter) }, 0)
}

/// Make `src` known to the engine: one heartbeat, at the start, so its slot names it.
fn introduce(engine: &mut FleetEngine, clock: &Clock, src: Mac) {
    engine.handle(heartbeat(src, 1), clock.at(0));
}

/// The bridge delivered `ToNode` `id`.
fn delivered(id: u16) -> Event {
    Event::Link(LinkEvent::Message(BridgeToHost::Delivered { id }))
}

/// The bridge could not deliver `ToNode` `id`.
fn undelivered(id: u16, reason: Undelivered) -> Event {
    Event::Link(LinkEvent::Message(BridgeToHost::Undelivered { id, reason }))
}

/// Every record for a node in a batch, as (id, slot, type, body).
fn to_nodes(batch: &ActionBatch) -> Vec<(u16, Slot, u8, Vec<u8>)> {
    batch
        .urgent
        .iter()
        .filter_map(|action| match action {
            HostToBridge::ToNode { slot, id, kind, body } => {
                Some((*id, *slot, *kind, body.to_vec()))
            }
            _ => None,
        })
        .collect()
}

/// Answer every record for a node in `batch` with a delivery, so none is still in flight.
fn deliver_all(engine: &mut FleetEngine, batch: &ActionBatch, now: Now) {
    for (id, ..) in to_nodes(batch) {
        engine.handle(delivered(id), now);
    }
}

/// The one assignment in a batch, decoded back off the wire, with the node seated in
/// the slot it went to.
fn sent_admin(batch: &ActionBatch) -> (u16, Mac, Assign) {
    assert_eq!(batch.urgent.len(), 1, "exactly one assignment");
    let HostToBridge::ToNode { id, slot, kind, body } = &batch.urgent[0] else {
        panic!("expected a ToNode")
    };
    assert_eq!(*kind, DownType::Assign.as_u8(), "an assignment");
    (*id, seated(*slot), Assign::decode(body).expect("a valid assignment"))
}

/// Every assignment in a batch, decoded back off the wire.
///
/// Unlike [`sent_admin`], says nothing about how many there should be: a re-cut can
/// send one per node whose heartbeat happens to arrive.
fn admins(batch: &ActionBatch) -> Vec<Assign> {
    to_nodes(batch)
        .into_iter()
        .filter(|(_, _, kind, _)| *kind == DownType::Assign.as_u8())
        .map(|(_, _, _, body)| Assign::decode(&body).expect("a valid assignment"))
        .collect()
}

/// The one clear in a batch, as (id, node).
///
/// Filters rather than indexing `batch.urgent[0]`: a heartbeat that also owes an
/// assignment can carry both, and this says nothing about that one.
fn sent_clear(batch: &ActionBatch) -> (u16, Mac) {
    let found = clears(batch);
    assert_eq!(found.len(), 1, "exactly one clear");
    found[0]
}

/// Every clear in a batch, as (id, node).
fn clears(batch: &ActionBatch) -> Vec<(u16, Mac)> {
    to_nodes(batch)
        .into_iter()
        .filter(|(_, _, kind, _)| *kind == DownType::Clear.as_u8())
        .map(|(id, slot, _, body)| {
            assert!(body.is_empty(), "a clear has no body");
            (id, seated(slot))
        })
        .collect()
}

/// A sighting batch carrying `records`, in order, under `seq`.
fn batch(seq: u16, records: &[SightingMsg<'_>]) -> Vec<u8> {
    let mut writer = SightingBatchWriter::new(seq);
    for msg in records {
        assert!(writer.push(msg), "test record fits");
    }
    writer.as_bytes().to_vec()
}

fn observation(src: Mac, bssid: &str, rssi: i8) -> Event {
    named_observation(src, bssid, rssi, b"example")
}

fn named_observation(src: Mac, bssid: &str, rssi: i8, ssid: &[u8]) -> Event {
    let mut raw = [0u8; 6];
    for (byte, hex) in raw.iter_mut().zip(bssid.split(':')) {
        *byte = u8::from_str_radix(hex, 16).expect("a hex octet");
    }
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: raw,
        channel: 6,
        rssi,
        security: Security::Wpa2Psk,
        ssid,
        ext: &[],
    };
    sightings(src, &batch(1, &[msg]))
}

/// A node's update status from the last slot, stamped `rx_us`: a record the engine reads
/// nothing of, so it files nobody.
fn stray(rx_us: u32) -> Event {
    let status = UpdateStatus { session: 1, state: UpdateState::Idle, next_offset: 0 };
    let slot = Slot::from_u8(u8::try_from(MAX_SLOTS - 1).expect("fits")).expect("a slot");
    from_slot(slot, UpType::UpdateStatus.as_u8(), &status.encode(), rx_us)
}

fn connected() -> Event {
    connected_with_mac([0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24])
}

/// A connect from a bridge at `mac`, for the tests about a bridge swap.
fn connected_with_mac(mac: Mac) -> Event {
    connected_board(mac, NodeBoard::NoCarrier)
}

/// A connect from a bridge whose nodes `board` carries.
fn connected_board(mac: Mac, node_board: NodeBoard) -> Event {
    Event::Link(LinkEvent::Connected(BridgeInfo {
        mac,
        fw_version: "0.1.0".to_owned(),
        // An ordinary connect. The engine draws no conclusions from these, but a
        // fixture that said otherwise would describe a fleet without a bridge.
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        uptime_ms: 1_000,
        node_board,
        radio: None,
        image: None,
    }))
}

/// Connected, with one record in at bridge time 0, so the next record's stamp is
/// timed against it: a test about a host behind the bridge stamps its records ahead of
/// the host's clock. The record is a [`stray`], so it files nobody.
fn caught_up(engine: &mut FleetEngine, clock: &Clock) {
    engine.handle(connected(), clock.at(0));
    engine.handle(stray(0), clock.at(0));
}

fn counters(engine: &FleetEngine) -> Counters {
    engine.counters()
}

#[test]
fn engine_records_node_and_observation_when_sighting_arrives() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));

    assert_eq!(batch.records.len(), 2, "one node touch and one observation");
    assert!(matches!(batch.records[0], Record::Node(_)));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };
    assert_eq!(obs.bssid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    assert_eq!(obs.rssi, -60);
    assert_eq!(obs.security, "[WPA2_PSK]");
    assert_eq!(obs.rx_at_ms, EPOCH_MS + 1000);
    assert_eq!(obs.node_mac, NODE, "filed under the node its slot's heartbeat named");
    assert_eq!(obs.slot, slot_of(NODE));
    assert_eq!(counters(&engine).observations, 1);
}

#[test]
fn engine_counts_unique_addresses_once_per_kind_when_heard_repeatedly() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let sighting = |kind: RecordKind, n: u8| {
        let msg = SightingMsg {
            kind,
            bssid: [0x02, 0x00, 0x00, 0x00, u8::from(kind == RecordKind::Ble), n],
            channel: 6,
            rssi: -60,
            security: Security::Wpa2Psk,
            ssid: b"example",
            ext: &[],
        };
        sightings(NODE, &batch(1, &[msg]))
    };
    for round in 0..3 {
        for n in 0..30 {
            engine.handle(sighting(RecordKind::Wifi, n), clock.at(round));
        }
        for n in 0..7 {
            engine.handle(sighting(RecordKind::Ble, n), clock.at(round));
        }
    }

    // Within one rather than exact: two of these addresses sharing one of the estimator's
    // 16,384 registers reads one short, and whether any do depends on a hash the standard
    // library is free to change.
    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    assert!(snapshot.unique_wifi_aps.abs_diff(30) <= 1, "{}", snapshot.unique_wifi_aps);
    assert!(snapshot.unique_ble_aps.abs_diff(7) <= 1, "{}", snapshot.unique_ble_aps);
    assert_eq!(counters(&engine).observations, 111, "every sighting still counts");
}

#[test]
fn engine_keeps_record_bytes_in_raw_body_when_sighting_recorded() {
    // If this decoder turns out to be wrong, the record as it arrived is what lets
    // the fix reach history rather than only what comes afterwards.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));
    let Record::Observation(obs) = &batch.records[1] else { panic!("expected an observation") };

    // The record's own bytes, not the batch's shared header, which a re-parse
    // does not need. The whole batch is kept separately under `--record-raw`.
    let (decoded, consumed) = SightingMsg::decode_record(&obs.raw_body).expect("valid");
    assert_eq!(consumed, obs.raw_body.len(), "nothing but this one record");
    assert_eq!(decoded.ssid, b"example");
}

/// An engine that never captures `HomeNet`.
fn excluding_home(clock: &Clock) -> FleetEngine {
    engine(
        EngineConfig {
            excluded_ssids: IndexSet::from([b"HomeNet".to_vec()]),
            ..Default::default()
        },
        clock,
    )
}

fn observations(batch: &ActionBatch) -> usize {
    batch.records.iter().filter(|record| matches!(record, Record::Observation(_))).count()
}

#[test]
fn engine_drops_wifi_sighting_when_ssid_is_excluded() {
    let clock = Clock::new();
    let mut engine = excluding_home(&clock);
    introduce(&mut engine, &clock, NODE);

    let batch =
        engine.handle(named_observation(NODE, "AA:BB:CC:DD:EE:FF", -60, b"HomeNet"), clock.at(1));

    assert_eq!(observations(&batch), 0, "nothing reaches the store");
    assert!(matches!(batch.records[..], [Record::Node(_)]), "the node is still heard");
    assert_eq!(counters(&engine).observations, 0);
    let snapshot = engine.snapshot(clock.at(1), StoreStats::default());
    assert!(snapshot.tail.is_empty(), "nothing reaches the live stream");
    assert_eq!(snapshot.unique_wifi_aps, 0);
    assert_eq!(snapshot.excluded_ssids, ["HomeNet"], "the modal sees what is in force");
}

#[test]
fn engine_keeps_sighting_when_ssid_differs_only_in_case() {
    let clock = Clock::new();
    let mut engine = excluding_home(&clock);
    introduce(&mut engine, &clock, NODE);

    let batch =
        engine.handle(named_observation(NODE, "AA:BB:CC:DD:EE:FF", -60, b"homenet"), clock.at(1));

    assert_eq!(observations(&batch), 1, "the match is exact");
    assert_eq!(counters(&engine).observations, 1);
}

#[test]
fn engine_keeps_ble_sighting_when_name_matches_excluded_ssid() {
    // A Bluetooth name is not an SSID, so the list never applies to one.
    let clock = Clock::new();
    let mut engine = excluding_home(&clock);
    introduce(&mut engine, &clock, NODE);
    let msg = SightingMsg {
        kind: RecordKind::Ble,
        bssid: [0x02, 0x00, 0x00, 0x00, 0x01, 0x01],
        channel: 6,
        rssi: -60,
        security: Security::Wpa2Psk,
        ssid: b"HomeNet",
        ext: &[],
    };

    let batch = engine.handle(sightings(NODE, &batch(1, &[msg])), clock.at(1));

    assert_eq!(observations(&batch), 1);
    assert_eq!(counters(&engine).observations, 1);
}

#[test]
fn engine_drops_sighting_when_ssid_excluded_mid_run_by_command() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);
    let before =
        engine.handle(named_observation(NODE, "AA:BB:CC:DD:EE:01", -60, b"HomeNet"), clock.at(1));
    assert_eq!(observations(&before), 1, "captured until excluded");

    engine.handle(
        Event::Command(Command::SetExcludedSsids { ssids: IndexSet::from([b"HomeNet".to_vec()]) }),
        clock.at(2),
    );
    let after =
        engine.handle(named_observation(NODE, "AA:BB:CC:DD:EE:02", -60, b"HomeNet"), clock.at(3));

    assert_eq!(observations(&after), 0);
    assert_eq!(counters(&engine).observations, 1, "only the sighting before the command");
}

#[test]
fn engine_records_one_node_and_each_observation_when_batch_arrives() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msgs = [
        SightingMsg {
            kind: RecordKind::Wifi,
            bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01],
            channel: 6,
            rssi: -50,
            security: Security::Open,
            ssid: b"one",
            ext: &[],
        },
        SightingMsg {
            kind: RecordKind::Wifi,
            bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02],
            channel: 6,
            rssi: -55,
            security: Security::Open,
            ssid: b"two",
            ext: &[],
        },
        SightingMsg {
            kind: RecordKind::Ble,
            bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x03],
            channel: 0,
            rssi: -60,
            security: Security::Ble,
            ssid: b"",
            ext: &[],
        },
    ];
    let frame = batch(1, &msgs);
    let result = engine.handle(sightings(NODE, &frame), clock.at(1));

    // One touch for the whole batch, not one per record.
    assert_eq!(result.records.len(), 4, "one node touch and three observations");
    assert!(matches!(result.records[0], Record::Node(_)));
    let observations: Vec<_> = result.records[1..]
        .iter()
        .map(|r| match r {
            Record::Observation(obs) => obs,
            other => panic!("expected an observation, got {other:?}"),
        })
        .collect();
    assert_eq!(observations.len(), 3);
    for (obs, msg) in observations.iter().zip(&msgs) {
        assert_eq!(obs.bssid, msg.bssid);
        // Each observation's raw_body is its own record, never the batch
        // around it.
        let (decoded, consumed) = SightingMsg::decode_record(&obs.raw_body).expect("valid");
        assert_eq!(consumed, obs.raw_body.len());
        assert_eq!(decoded.bssid, msg.bssid);
    }
    assert_eq!(counters(&engine).observations, 3);
}

#[test]
fn engine_counts_batches_lost_when_seq_skips_two() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    engine.handle(sightings(NODE, &batch(1, &[msg])), clock.at(1));
    engine.handle(sightings(NODE, &batch(4, &[msg])), clock.at(2));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(node.batches_lost, 2, "batches 2 and 3 never arrived");
    assert_eq!(counters(&engine).batches_lost, 2);
}

#[test]
fn engine_counts_no_loss_when_seq_repeats_with_new_bytes() {
    // A node whose send fails after all its tries even though the batch
    // arrived does not advance seq, so its next batch reuses the number with
    // new content rather than resending what already landed. Different bytes
    // at the same seq, so this is not the byte-identical copy case.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let other = SightingMsg { bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01], ..msg };
    engine.handle(sightings(NODE, &batch(1, &[msg])), clock.at(1));
    engine.handle(sightings(NODE, &batch(1, &[other])), clock.at(2));
    engine.handle(sightings(NODE, &batch(2, &[msg])), clock.at(3));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(node.batches_lost, 0, "a repeated seq with new bytes is not a gap");
    assert_eq!(counters(&engine).batches_lost, 0);
    assert_eq!(counters(&engine).duplicate_batches, 0, "different bytes, not a retransmission");
    assert_eq!(counters(&engine).observations, 3, "every batch's record is kept");
}

/// The `BatchGap` records in `batch`, in order.
fn gaps(batch: &ActionBatch) -> Vec<BatchGap> {
    batch
        .records
        .iter()
        .filter_map(|r| match r {
            Record::BatchGap(gap) => Some(*gap),
            _ => None,
        })
        .collect()
}

/// A one-record batch under `seq`, whose bytes differ with `bssid_tail` so no two
/// are mistaken for a retransmission.
fn seq_batch(seq: u16, bssid_tail: u8) -> Vec<u8> {
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, bssid_tail],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    batch(seq, &[msg])
}

#[test]
fn engine_records_batch_gap_when_seq_jumps() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    introduce(&mut engine, &clock, NODE);

    assert!(gaps(&engine.handle(sightings(NODE, &seq_batch(5, 1)), clock.at(1))).is_empty());
    let found = gaps(&engine.handle(sightings(NODE, &seq_batch(8, 2)), clock.at(2)));

    assert_eq!(
        found,
        vec![BatchGap {
            node_mac: NODE,
            rx_at_ms: clock.at(2).unix_ms,
            after_seq: 5,
            seq: 8,
            lost: 2
        }]
    );
    assert_eq!(counters(&engine).batches_lost, 2, "the live count is unchanged");
}

#[test]
fn engine_ignores_batch_gap_when_previous_batch_replayed() {
    // A host behind the bridge reads batch 25 late. What the node sent between it and batch 84
    // went while the bridge's queue overflowed, and the bridge's own drop count covers that.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    introduce(&mut engine, &clock, NODE);

    let replayed = engine.handle(sightings_at(NODE, &seq_batch(25, 1), 1_000_000), clock.at_ms(10));
    // The host out-waits the bridge, so this one is live.
    let live = engine.handle(sightings_at(NODE, &seq_batch(84, 2), 1_500_000), clock.at_ms(1_010));

    assert!(gaps(&replayed).is_empty());
    assert!(gaps(&live).is_empty(), "the gap starts at a replayed batch");
    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(node.batches_lost, 0);
    assert_eq!(counters(&engine).batches_lost, 0);
    assert_eq!(node.last_seq, Some(84), "the baseline still moves");
}

#[test]
fn engine_counts_batch_gap_when_previous_batch_live() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    introduce(&mut engine, &clock, NODE);

    engine.handle(sightings_at(NODE, &seq_batch(25, 1), 1_000_000), clock.at_ms(10));
    engine.handle(sightings_at(NODE, &seq_batch(84, 2), 1_500_000), clock.at_ms(1_010));
    let found =
        gaps(&engine.handle(sightings_at(NODE, &seq_batch(87, 3), 2_000_000), clock.at_ms(1_510)));

    assert_eq!(found.len(), 1);
    assert_eq!((found[0].after_seq, found[0].seq, found[0].lost), (84, 87, 2));
    assert_eq!(counters(&engine).batches_lost, 2);
}

#[test]
fn engine_records_no_batch_gap_when_seq_repeats() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    engine.handle(sightings(NODE, &seq_batch(5, 1)), clock.at(1));
    let repeat = engine.handle(sightings(NODE, &seq_batch(5, 2)), clock.at(2));
    let next = engine.handle(sightings(NODE, &seq_batch(6, 3)), clock.at(3));

    assert!(gaps(&repeat).is_empty());
    assert!(gaps(&next).is_empty());
}

#[test]
fn engine_records_no_batch_gap_when_gap_reaches_uncounted_gap() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    introduce(&mut engine, &clock, NODE);

    engine.handle(sightings(NODE, &seq_batch(5, 1)), clock.at(1));
    // 1030 - 5 - 1 = 1024: the first gap the live count also refuses.
    let at_limit = engine.handle(sightings(NODE, &seq_batch(1030, 2)), clock.at(2));
    // Backwards, which wraps to a gap of 65534.
    let backwards = engine.handle(sightings(NODE, &seq_batch(1000, 3)), clock.at(3));

    assert!(gaps(&at_limit).is_empty());
    assert!(gaps(&backwards).is_empty());
    assert_eq!(counters(&engine).batches_lost, 0);
}

#[test]
fn engine_records_batch_gap_when_seq_wraps() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    introduce(&mut engine, &clock, NODE);

    engine.handle(sightings(NODE, &seq_batch(65535, 1)), clock.at(1));
    let found = gaps(&engine.handle(sightings(NODE, &seq_batch(1, 2)), clock.at(2)));

    assert_eq!(found.len(), 1);
    assert_eq!((found[0].after_seq, found[0].seq, found[0].lost), (65535, 1, 1), "0 was lost");
}

#[test]
fn engine_drops_batch_when_seq_and_bytes_repeat() {
    // The same batch forwarded twice inside the window: a copy, not a second
    // report.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let frame = batch(1, &[msg]);
    engine.handle(sightings_at(NODE, &frame, 0), clock.at(1));
    // A copy a few ms later by the bridge's own clock.
    engine.handle(sightings_at(NODE, &frame, 4_000), clock.at(2));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(counters(&engine).observations, 1, "the duplicate's record is not recorded twice");
    assert_eq!(counters(&engine).duplicate_batches, 1);
    assert_eq!(counters(&engine).batches_lost, 0);
    assert_eq!(node.duplicate_batches, 1);
    assert_eq!(
        node.last_seen_ms,
        EPOCH_MS + 2000,
        "the duplicate is still proof of life, so last_seen moves to its arrival"
    );
}

#[test]
fn engine_drops_batch_when_repeat_inside_window() {
    // The node saw no ack, so the bus resends the batch after its 1 s ack timeout.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let frame = batch(1, &[msg]);
    engine.handle(sightings_at(NODE, &frame, 0), clock.at(1));
    engine.handle(sightings_at(NODE, &frame, 1_000_000), clock.at(2));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(counters(&engine).observations, 1, "the duplicate's record is not recorded twice");
    assert_eq!(counters(&engine).duplicate_batches, 1);
    assert_eq!(counters(&engine).batches_lost, 0);
    assert_eq!(node.duplicate_batches, 1);
}

#[test]
fn engine_drops_batch_when_repeat_inside_window_of_last_copy() {
    // Three tries: each copy is inside the window of the one before, though the third is past
    // the first's.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let frame = batch(1, &[msg]);
    engine.handle(sightings_at(NODE, &frame, 0), clock.at(1));
    engine.handle(sightings_at(NODE, &frame, 1_500_000), clock.at(2));
    engine.handle(sightings_at(NODE, &frame, 3_000_000), clock.at(3));

    assert_eq!(counters(&engine).observations, 1, "only the first copy is recorded");
    assert_eq!(counters(&engine).duplicate_batches, 2);
}

#[test]
fn engine_records_batch_when_repeat_after_window() {
    // The node sent batch 1 and the bridge received it, but every ack was
    // lost, so the node gave up and left `seq` at 1. The same addresses are
    // due again and go out under that seq in its next batch; heard at the
    // same RSSI, the bytes are identical too. Past the window this is a later
    // sighting, not a copy, and is recorded.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let frame = batch(1, &[msg]);
    engine.handle(sightings_at(NODE, &frame, 0), clock.at(1));
    // The window is 2 s, compared with a strict `<`.
    engine.handle(sightings_at(NODE, &frame, 2_000_000), clock.at(2));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(counters(&engine).observations, 2, "both batches are recorded");
    assert_eq!(counters(&engine).duplicate_batches, 0);
    assert_eq!(node.duplicate_batches, 0);
}

#[test]
fn engine_records_raw_frame_when_batch_duplicate() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig { record_raw: true, ..Default::default() }, &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let frame = batch(1, &[msg]);
    engine.handle(sightings(NODE, &frame), clock.at(1));
    let second = engine.handle(sightings(NODE, &frame), clock.at(2));

    assert!(
        second.records.iter().any(|r| matches!(r, Record::Raw(_))),
        "a duplicate is still a record the bridge forwarded, kept under --record-raw"
    );
}

#[test]
fn engine_resets_seq_baseline_when_node_reboots() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    engine.handle(heartbeat(NODE, 5), clock.at(1));
    engine.handle(sightings(NODE, &batch(10, &[msg])), clock.at(2));

    // The node reboots: its heartbeat counter goes back to a boot value, and
    // its batch counter along with it.
    engine.handle(heartbeat(NODE, 1), clock.at(3));
    engine.handle(sightings(NODE, &batch(0, &[msg])), clock.at(4));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!(node.batches_lost, 0, "the gap across a reboot is history, not a loss");
    assert_eq!(node.last_seq, Some(0));
}

#[test]
fn engine_records_batch_when_it_repeats_after_reboot() {
    // The reboot arm clears the stored batch bytes along with last_seq, so a
    // batch that happens to repeat the pre-reboot seq and bytes is a fresh
    // observation rather than a retransmission of what came before the boot.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    engine.handle(heartbeat(NODE, 5), clock.at(1));
    engine.handle(sightings(NODE, &batch(3, &[msg])), clock.at(2));

    // The node reboots: its heartbeat counter goes back to a boot value.
    engine.handle(heartbeat(NODE, 1), clock.at(3));
    // Same seq, same bytes as the batch before the reboot.
    engine.handle(sightings(NODE, &batch(3, &[msg])), clock.at(4));

    assert_eq!(counters(&engine).duplicate_batches, 0, "a reboot clears the baseline");
    assert_eq!(counters(&engine).observations, 2, "both batches are recorded");
}

#[test]
fn engine_increments_reboot_counter_when_heartbeat_counter_decreases() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat(NODE, 174), clock.at(1));
    engine.handle(heartbeat(NODE, 175), clock.at(2));
    assert_eq!(engine.nodes().next().expect("a node").reboots, 0);

    engine.handle(heartbeat(NODE, 2), clock.at(3));

    let node = engine.nodes().next().expect("a node");
    assert_eq!(node.reboots, 1);
    assert_eq!(node.counter, Some(2), "the new counter is adopted, not rejected");
}

#[test]
fn engine_ignores_refusals_when_first_heartbeat_of_session() {
    // What a node refused before this capture started is not this capture's.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat_refusing(NODE, 10, 500, 70), clock.at(1));

    assert_eq!((counters(&engine).wifi_refused, counters(&engine).ble_refused), (0, 0));
}

#[test]
fn engine_sums_refusals_across_nodes_when_heartbeats_advance() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat_refusing(NODE, 10, 100, 0), clock.at(1));
    engine.handle(heartbeat_refusing(OTHER, 20, 7, 0), clock.at(1));
    engine.handle(heartbeat_refusing(NODE, 11, 103, 0), clock.at(2));
    engine.handle(heartbeat_refusing(OTHER, 21, 12, 0), clock.at(2));
    engine.handle(heartbeat_refusing(NODE, 12, 104, 0), clock.at(3));

    assert_eq!(counters(&engine).wifi_refused, 3 + 5 + 1);
}

#[test]
fn engine_keeps_wifi_and_ble_refusals_apart_when_both_advance() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat_refusing(NODE, 10, 100, 40), clock.at(1));
    engine.handle(heartbeat_refusing(NODE, 11, 102, 49), clock.at(2));

    assert_eq!(counters(&engine).wifi_refused, 2);
    assert_eq!(counters(&engine).ble_refused, 9);
}

#[test]
fn engine_counts_since_boot_refusals_when_node_reboots() {
    // A reboot restarts the node's count at 0, so everything it carries after
    // one was refused this session.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat_refusing(NODE, 174, 100, 40), clock.at(1));
    engine.handle(heartbeat_refusing(NODE, 175, 110, 41), clock.at(2));
    engine.handle(heartbeat_refusing(NODE, 2, 6, 1), clock.at(3));
    engine.handle(heartbeat_refusing(NODE, 3, 8, 1), clock.at(4));

    assert_eq!(engine.nodes().next().expect("a node").reboots, 1);
    assert_eq!(counters(&engine).wifi_refused, 10 + 6 + 2);
    assert_eq!(counters(&engine).ble_refused, 1 + 1);
}

#[test]
fn engine_reads_restart_when_refusals_fall_without_reboot() {
    // Heard soon after boot, then rebooted out of range and heard again at a
    // higher counter: neither the counter nor the epoch shows the reboot, but a
    // since-boot count that falls does.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat_refusing(NODE, 3, 40, 9), clock.at(1));
    engine.handle(heartbeat_refusing(NODE, 10, 2, 1), clock.at(2));

    assert_eq!(counters(&engine).wifi_refused, 2);
    assert_eq!(counters(&engine).ble_refused, 1);
    assert_eq!(
        engine.nodes().next().expect("a node").reboots,
        0,
        "refusals are not the reboot detector"
    );
}

#[test]
fn engine_undercounts_refusals_when_count_wraps() {
    // A wrap looks like a restart, so the refusals between the last count and the
    // wrap are lost. That is at most one heartbeat's worth once every 65536
    // refusals, where reading a missed reboot as a wrap would add ~65000.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat_refusing(NODE, 10, u16::MAX - 1, u16::MAX), clock.at(1));
    engine.handle(heartbeat_refusing(NODE, 11, 3, 0), clock.at(2));

    assert_eq!(counters(&engine).wifi_refused, 3);
    assert_eq!(counters(&engine).ble_refused, 0);
}

#[test]
fn engine_keeps_node_seen_but_not_alive_when_only_sightings_arrive() {
    // Two questions: is this node there, and can it still be given a range.
    let clock = Clock::new();
    let config = EngineConfig { topology_timeout: Duration::from_secs(60), ..Default::default() };
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
fn engine_counts_undecodable_and_records_nothing_when_truncated() {
    // A batch that arrives short is not an observation of anything.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Wpa2Psk,
        ssid: b"example",
        ext: &[],
    };
    let body = batch(1, &[msg]);
    let batch = engine.handle(sightings(NODE, &body[..body.len() - 1]), clock.at(1));

    assert_eq!(counters(&engine).undecodable, 1);
    assert_eq!(counters(&engine).observations, 0);
    assert!(batch.records.is_empty(), "and nothing was recorded at all");
}

#[test]
fn engine_counts_undecodable_when_record_type_unknown() {
    // A type byte this build has no name for, from a slot a heartbeat named: counted, and
    // nothing filed or recorded for it.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);
    let before = counters(&engine);

    let batch = engine.handle(from_slot(slot_of(NODE), 0x7F, b"", 0), clock.at(1));

    assert_eq!(counters(&engine).undecodable, 1);
    assert_eq!(counters(&engine).frames, before.frames + 1, "still a record the bridge forwarded");
    assert_eq!(counters(&engine).heartbeats, before.heartbeats);
    assert!(batch.is_empty(), "nothing recorded and nothing sent");
    assert_eq!(engine.nodes().count(), 1, "the node is the one its heartbeat filed");
}

#[test]
fn engine_records_reset_cause_when_node_restarts() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 175), clock.at(1));

    // The counter falling is the restart; the cause is what the new boot says it was.
    let msg = HeartbeatMsg { reset_cause: ResetCause::Brownout, ..beat_msg(NODE, 2) };
    let batch = engine.handle(beat(msg, 0), clock.at(2));

    let beats: Vec<(Slot, ResetCause)> = batch
        .records
        .iter()
        .filter_map(|r| match r {
            Record::Heartbeat(h) => Some((h.slot, h.reset_cause)),
            _ => None,
        })
        .collect();
    assert_eq!(beats, [(slot_of(NODE), ResetCause::Brownout)]);
    assert_eq!(engine.nodes().next().expect("one node").reboots, 1);
}

/// A heartbeat carrying the given health readings.
fn heartbeat_reading(src: Mac, counter: u32, vdd: Vdd, die_temp: DieTemp) -> Event {
    beat(HeartbeatMsg { vdd, die_temp, ..beat_msg(src, counter) }, 0)
}

/// The health readings each heartbeat record in `batch` carries.
fn recorded_health(batch: &wartui_core::ActionBatch) -> Vec<(Option<u16>, Option<i16>)> {
    batch
        .records
        .iter()
        .filter_map(|r| match r {
            Record::Heartbeat(h) => Some((h.vdd_mv, h.die_temp_c)),
            _ => None,
        })
        .collect()
}

#[test]
fn engine_records_health_readings_when_heartbeat_carries_them() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let batch = engine
        .handle(heartbeat_reading(NODE, 1, Vdd::from_mv(3304), DieTemp::from_c(41)), clock.at(1));

    assert_eq!(recorded_health(&batch), [(Some(3304), Some(41))]);
    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!((node.vdd, node.die_temp), (Vdd::from_mv(3304), DieTemp::from_c(41)));
}

#[test]
fn engine_clears_health_readings_when_later_heartbeat_carries_none() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat_reading(NODE, 1, Vdd::from_mv(3304), DieTemp::from_c(41)), clock.at(1));
    // A failed read is not the last good one: the node state shows what was sent.
    let batch = engine.handle(heartbeat_reading(NODE, 2, Vdd::NONE, DieTemp::NONE), clock.at(2));

    assert_eq!(recorded_health(&batch), [(None, None)]);
    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert_eq!((node.vdd, node.die_temp), (Vdd::NONE, DieTemp::NONE));
}

/// What a node a minor version behind this build announces.
fn outdated_capabilities() -> Capabilities {
    let version = minor_behind(NODE_VERSION);
    Capabilities { version, ..Capabilities::here(Board::Esp32c5) }
}

/// What a node a minor version ahead of this build announces.
fn ahead_capabilities() -> Capabilities {
    let minor = NODE_VERSION.minor.checked_add(1).expect("a minor after this one");
    Capabilities {
        version: Version { minor, ..NODE_VERSION },
        ..Capabilities::here(Board::Esp32c5)
    }
}

#[test]
fn engine_files_node_as_unassignable_when_heartbeat_names_another_minor() {
    // Exactly this build's length, but a minor version may move fields without resizing
    // the body, so only the prefix and the MAC are believed.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(0));
    let batch = engine.handle(beat_at(NODE, 1, 0, outdated_capabilities(), 0), clock.at(1));

    assert!(admins(&batch).is_empty(), "nothing is sent to a node that needs an update");
    assert_eq!(counters(&engine).incompatible, 0, "a named node, not an anonymous record");
    assert_eq!(counters(&engine).heartbeats, 0);
    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node is filed");
    assert_eq!(node.capabilities, Some(outdated_capabilities()));
    assert!(!engine.is_assignable(node, clock.at(1)));
    let snapshot = engine.snapshot(clock.at(1), StoreStats::default());
    assert_eq!(snapshot.nodes.len(), 1, "shown in the fleet table");
    assert!(!snapshot.nodes[0].assignable);
}

#[test]
fn engine_files_node_from_prefix_when_heartbeat_of_another_minor_has_another_length() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let caps = outdated_capabilities();
    // Shorter than this build's, but long enough for the prefix and the MAC.
    let mut body = beat_body(NODE, caps);
    body.truncate(14);
    let batch = engine.handle(rx(NODE, UpType::Heartbeat, &body), clock.at(1));

    assert_eq!(counters(&engine).incompatible, 0);
    assert_eq!(counters(&engine).undecodable, 0);
    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node is filed");
    assert_eq!(node.capabilities, Some(caps));
    assert_eq!(node.slot, Some(slot_of(NODE)), "where an update reaches it");
    assert!(!engine.is_assignable(node, clock.at(1)));
    let filed = batch.records.iter().find_map(|record| match record {
        Record::Node(seen) => seen.capabilities.clone(),
        _ => None,
    });
    assert_eq!(filed, Some(caps.to_string()), "the store names its version");
}

#[test]
fn engine_counts_incompatible_when_heartbeat_of_this_minor_has_another_length() {
    // A node on this minor that sends the wrong length is a bug, not an older node.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let mut body = beat_body(NODE, Capabilities::here(Board::Esp32c5));
    body.push(0);
    engine.handle(rx(NODE, UpType::Heartbeat, &body), clock.at(1));

    assert_eq!(counters(&engine).incompatible, 1);
    assert_eq!(engine.nodes().count(), 0);
}

#[test]
fn engine_assigns_node_when_its_patch_differs() {
    // A patch never changes the wire, so a node on another patch is driven as usual.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let version = Version { patch: NODE_VERSION.patch.wrapping_add(3), ..NODE_VERSION };
    let caps = Capabilities { version, ..Capabilities::here(Board::Esp32c5) };
    engine.handle(beat_at(NODE, 1, 0, caps, 0), clock.at(1));

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    assert!(engine.is_assignable(node, clock.at(1)));
}

#[test]
fn engine_files_node_as_unassignable_when_heartbeat_names_a_newer_minor() {
    // The host is the one behind, but the node's layout is still not this build's.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(0));
    let batch = engine.handle(beat_at(NODE, 1, 0, ahead_capabilities(), 0), clock.at(1));

    assert!(admins(&batch).is_empty());
    assert_eq!(counters(&engine).incompatible, 0);
    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node is filed");
    assert_eq!(node.capabilities, Some(ahead_capabilities()));
    assert!(!engine.is_assignable(node, clock.at(1)));
}

#[test]
fn engine_drops_sightings_when_node_runs_another_minor() {
    // It may still hold an assignment from a build that could drive it, and its batches are
    // not this build's layout even when they parse.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(beat_at(NODE, 1, 0, outdated_capabilities(), 0), clock.at(1));
    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(2));

    assert_eq!(observations(&batch), 0, "nothing reaches the store");
    assert_eq!(counters(&engine).observations, 0);
    assert_eq!(counters(&engine).incompatible, 1);

    engine.handle(beat_at(OTHER, 1, 0, ahead_capabilities(), 0), clock.at(3));
    engine.handle(observation(OTHER, "AA:BB:CC:DD:EE:FF", -60), clock.at(4));
    assert_eq!(counters(&engine).observations, 0);
    assert_eq!(counters(&engine).incompatible, 2);
}

#[test]
fn engine_drops_sightings_when_no_heartbeat_has_named_slot() {
    // A batch carries no address, and nothing yet says whose slot it came from. A node's
    // first seconds on the bus look like this.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig { record_raw: true, ..Default::default() }, &clock);
    let batch = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));

    assert_eq!(counters(&engine).unknown_slot, 1);
    assert_eq!(counters(&engine).observations, 0);
    assert_eq!((counters(&engine).undecodable, counters(&engine).incompatible), (0, 0));
    assert_eq!(observations(&batch), 0, "nothing reaches the store");
    assert!(
        batch.records.iter().all(|r| matches!(r, Record::Raw(_))),
        "and no node row, only the raw record: {:?}",
        batch.records
    );
    assert_eq!(engine.nodes().count(), 0, "nobody to file it under");

    // Once a heartbeat names the slot, its batches are that node's.
    engine.handle(heartbeat(NODE, 1), clock.at(2));
    engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(3));
    assert_eq!(counters(&engine).observations, 1);
    assert_eq!(counters(&engine).unknown_slot, 1);
}

#[test]
fn engine_moves_node_when_heartbeat_arrives_from_another_slot() {
    // The MAC is the identity: unplugged and plugged in elsewhere, it is the same node.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let first = slot_of(NODE);
    let (id, ..) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    // Unplugged before the assignment landed.
    engine.handle(undelivered(id, Undelivered::NoNode), clock.at(1));

    let moved = Slot::new(1, 2).expect("B3");
    let batch = engine.handle(
        from_slot(moved, UpType::Heartbeat.as_u8(), &beat_msg(NODE, 2).encode(), 0),
        clock.at(2),
    );

    assert_eq!(engine.nodes().count(), 1, "one node, not two");
    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.slot, Some(moved));
    assert_eq!(node.heartbeats, 2);
    let sent: Vec<Slot> = to_nodes(&batch).iter().map(|(_, slot, ..)| *slot).collect();
    assert_eq!(sent, [moved], "answered through the slot it now sits in");

    // The slot it left names nobody now.
    engine.handle(from_slot(first, UpType::Sightings.as_u8(), &seq_batch(1, 1), 0), clock.at(3));
    assert_eq!(counters(&engine).unknown_slot, 1);
    assert_eq!(counters(&engine).observations, 0);
}

#[test]
fn engine_remaps_slot_when_heartbeat_names_another_node() {
    // A node swapped for another in the same slot: the slot is the newcomer's, and the
    // node that left keeps its row without a slot, so nothing meant for one reaches the other.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let slot = slot_of(NODE);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    let batch = engine.handle(
        from_slot(slot, UpType::Heartbeat.as_u8(), &beat_msg(OTHER, 1).encode(), 0),
        clock.at(2),
    );

    let gone = engine.nodes().find(|n| n.mac == NODE).expect("still in the table");
    assert_eq!(gone.slot, None);
    let newcomer = engine.nodes().find(|n| n.mac == OTHER).expect("filed");
    assert_eq!(newcomer.slot, Some(slot));
    let sent: Vec<Slot> = to_nodes(&batch).iter().map(|(_, slot, ..)| *slot).collect();
    assert_eq!(sent, [slot], "the newcomer's share goes to the slot");

    engine.handle(from_slot(slot, UpType::Sightings.as_u8(), &seq_batch(1, 1), 0), clock.at(3));
    assert_eq!(engine.nodes().find(|n| n.mac == OTHER).expect("filed").observations, 1);
    assert_eq!(engine.nodes().find(|n| n.mac == NODE).expect("in the table").observations, 0);
}

#[test]
fn engine_leaves_node_out_of_plan_when_another_node_takes_its_slot() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let slot = slot_of(NODE);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(
        from_slot(slot, UpType::Heartbeat.as_u8(), &beat_msg(OTHER, 1).encode(), 0),
        clock.at(2),
    );
    engine.handle(Event::Tick, clock.at(3));

    // Heartbeating inside the timeout, but unreachable: a share cut for it would be nobody's.
    let unslotted = engine.nodes().find(|n| n.mac == NODE).expect("still in the table");
    assert!(engine.is_alive(unslotted, clock.at(3)));
    assert!(!engine.is_assignable(unslotted, clock.at(3)));
    assert!(unslotted.desired.is_none());
    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 1);
    assert_eq!(snapshot.assignable, 1);
    let newcomer = engine.nodes().find(|n| n.mac == OTHER).expect("filed");
    assert_eq!(newcomer.desired.expect("dealt").channels, ChannelPool::Us.channels());
}

#[test]
fn engine_withdraws_ble_at_once_when_scanner_reports_another_minor() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    for n in 0..2 {
        let batch = engine.handle(heartbeat(peer(n), 1), clock.at(1));
        deliver_all(&mut engine, &batch, clock.at(1));
    }
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(2));

    // Well inside the topology timeout: the scan does not wait for the node to age out.
    engine.handle(beat_at(peer(0), 2, 0, outdated_capabilities(), 0), clock.at(3));

    assert_eq!(
        ble_state(&engine, clock.at(3)),
        (None, Some(peer(0)), true),
        "withdrawn, and still the preferred node"
    );
    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    let other = snapshot.nodes.iter().find(|n| n.state.mac == peer(1)).expect("the other node");
    assert!(other.state.desired.is_some_and(|a| !a.ble), "nobody else is handed the scan");

    // Updated, it takes the scan back as a returning preferred node does.
    let batch = engine.handle(heartbeat(peer(0), 3), clock.at(4));
    assert_eq!(ble_state(&engine, clock.at(4)).0, Some(peer(0)));
    assert!(admins(&batch).iter().any(|admin| admin.scan_ble()));
}

#[test]
fn engine_snapshot_counts_undrivable_nodes_when_heard_inside_topology_timeout() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(beat_at(NODE, 1, 0, outdated_capabilities(), 0), clock.at(1));
    engine.handle(beat_at(OTHER, 1, 0, ahead_capabilities(), 0), clock.at(1));
    engine.handle(heartbeat(THIRD, 1), clock.at(1));

    let snapshot = engine.snapshot(clock.at(2), StoreStats::default());
    assert_eq!((snapshot.needs_update, snapshot.host_behind, snapshot.alive), (1, 1, 1));

    // Gone quiet, they stop counting, as a silent node stops being alive.
    let later = engine.snapshot(clock.at(120), StoreStats::default());
    assert_eq!((later.needs_update, later.host_behind), (0, 0));
}

/// A heartbeat body from `src` carrying `capabilities`, otherwise empty.
fn beat_body(src: Mac, capabilities: Capabilities) -> Vec<u8> {
    HeartbeatMsg { capabilities, ..beat_msg(src, 1) }.encode().to_vec()
}

#[test]
fn engine_ignores_update_status_when_node_reports_one() {
    // A node's update status answers an update session, which `run` never holds.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);
    let before = counters(&engine);

    let status = UpdateStatus { session: 1, state: UpdateState::Idle, next_offset: 0 }.encode();
    let batch = engine.handle(rx(NODE, UpType::UpdateStatus, &status), clock.at(1));

    let c = counters(&engine);
    assert_eq!((c.undecodable, c.incompatible), (0, 0));
    assert_eq!(c.heartbeats, before.heartbeats);
    assert!(batch.is_empty());
}

#[test]
fn engine_records_bridge_when_connected() {
    // The capture row is written before any bridge has announced.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    let batch = engine.handle(connected(), clock.at(1));

    let Some(Record::Bridge(bridge)) = batch.records.first() else {
        panic!("expected a bridge record, got {:?}", batch.records)
    };
    assert_eq!(bridge.mac, [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24]);
    assert_eq!(bridge.fw_version, "0.1.0");
}

#[test]
fn engine_polls_bridge_when_connected_and_each_interval() {
    let clock = Clock::new();
    let config = EngineConfig { status_interval: Duration::from_secs(5), ..Default::default() };
    let mut engine = engine(config, &clock);

    let batch = engine.handle(connected(), clock.at(1));
    assert_eq!(batch.bulk, vec![HostToBridge::GetStatus]);

    // Not again straight away: the interval starts from the connect.
    assert!(engine.handle(Event::Tick, clock.at(3)).bulk.is_empty());
    assert_eq!(engine.handle(Event::Tick, clock.at(6)).bulk, vec![HostToBridge::GetStatus]);
}

/// A config whose chain holds both receivers, the bridge's and the host's.
fn config_with_receivers(bridge: &Gps, host: &Gps) -> EngineConfig {
    let position = PositionChain::empty().with_gps(host.clone()).with_bridge_gps(bridge.clone());
    EngineConfig { position, ..Default::default() }
}

/// The rate writes in `batch`.
fn gps_writes(batch: &ActionBatch) -> Vec<Vec<u8>> {
    batch
        .bulk
        .iter()
        .filter_map(|cmd| match cmd {
            HostToBridge::GpsWrite { bytes } => Some(bytes.to_vec()),
            _ => None,
        })
        .collect()
}

#[test]
fn engine_writes_gps_rate_on_every_poll_when_bridge_is_connected() {
    let clock = Clock::new();
    let config = EngineConfig {
        gps_rate_hz: 2,
        ..config_with_receivers(&Gps::detached(), &Gps::detached())
    };
    let mut engine = engine(config, &clock);
    assert_eq!(gps_writes(&engine.handle(connected(), clock.at(1))), [valset_rate(2)]);
    // The next poll repeats it: a dropped write has nothing else behind it.
    assert_eq!(gps_writes(&engine.handle(Event::Tick, clock.at(7))), [valset_rate(2)]);
}

#[test]
fn engine_writes_no_gps_rate_when_chain_has_no_bridge_receiver() {
    // `--no-gps`: the bridge's receiver is left as it booted.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    assert!(gps_writes(&engine.handle(connected(), clock.at(1))).is_empty());
}

/// Whether the snapshot at `now` reports on the bridge's receiver.
fn shows_bridge_gps(engine: &FleetEngine, now: Now) -> bool {
    engine.snapshot(now, StoreStats::default()).gps.expect("a receiver").via_bridge
}

/// A GSV with no satellites: a sentence that parses, so the receiver is talking without a fix.
const GSV: &[u8] = b"$GPGSV,1,1,00*79\r\n";

/// `GGA` as the bridge forwards it, terminated.
fn streamed_gga() -> Vec<u8> {
    let mut bytes = GGA.to_vec();
    bytes.extend_from_slice(b"\r\n");
    bytes
}

#[test]
fn engine_snapshot_shows_host_receiver_when_bridge_receiver_is_silent() {
    let clock = Clock::new();
    let (bridge, host) = (Gps::bridged(DEFAULT_MAX_AGE), Gps::detached());
    let mut engine = engine(config_with_receivers(&bridge, &host), &clock);
    assert!(!shows_bridge_gps(&engine, clock.at(1)), "no bridge yet");
    engine.handle(connected(), clock.at(1));
    // The host's search carries on while the bridge's receiver says nothing.
    assert!(!shows_bridge_gps(&engine, clock.at(2)));
}

#[test]
fn engine_snapshot_shows_bridge_receiver_when_it_sends_no_fix_sentences() {
    // A receiver with no sky still talks every epoch, and the view follows it finding satellites.
    let clock = Clock::new();
    let (bridge, host) = (Gps::bridged(DEFAULT_MAX_AGE), Gps::detached());
    let mut engine = engine(config_with_receivers(&bridge, &host), &clock);
    engine.handle(connected(), clock.at(1));
    bridge.feed_bytes(GSV, clock.at(2).unix_ms);
    assert!(shows_bridge_gps(&engine, clock.at(2)));
    assert!(!shows_bridge_gps(&engine, clock.at(8)), "the last sentence is past the age limit");
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(3),
    );
    assert!(!shows_bridge_gps(&engine, clock.at(3)), "the host's search resumes");
}

#[test]
fn engine_snapshot_shows_pinned_host_receiver_when_bridge_receiver_is_talking() {
    // `--gps PATH` positions the rows ahead of the bridge, so the view reports on it.
    let clock = Clock::new();
    let bridge = Gps::bridged(DEFAULT_MAX_AGE);
    let host = Gps::spawn(GpsConfig::pinned("/dev/wartui-test-no-such-port").at_baud(9_600));
    let mut engine = engine(config_with_receivers(&bridge, &host), &clock);
    engine.handle(connected(), clock.at(1));
    bridge.feed_bytes(&streamed_gga(), clock.at(2).unix_ms);
    assert!(!shows_bridge_gps(&engine, clock.at(2)));
    host.stop();
}

#[test]
fn engine_snapshot_shows_bridge_receiver_when_chain_has_no_host_receiver_and_bridge_connects() {
    // `--gps bridge`: the bridge's receiver is the only one, so it is shown connecting before its
    // first fix.
    let clock = Clock::new();
    let position = PositionChain::empty().with_bridge_gps(Gps::bridged(DEFAULT_MAX_AGE));
    let mut engine = engine(EngineConfig { position, ..Default::default() }, &clock);
    engine.handle(connected(), clock.at(1));
    assert!(shows_bridge_gps(&engine, clock.at(2)));
}

#[test]
fn engine_snapshot_shows_no_receiver_when_chain_has_no_host_receiver_and_no_bridge_is_connected() {
    // Nothing is being read, so nothing is shown rather than a receiver that never connects.
    let clock = Clock::new();
    let position = PositionChain::empty().with_bridge_gps(Gps::bridged(DEFAULT_MAX_AGE));
    let mut engine = engine(EngineConfig { position, ..Default::default() }, &clock);
    let gps = |engine: &FleetEngine| engine.snapshot(clock.at(2), StoreStats::default()).gps;
    assert_eq!(gps(&engine), None, "no bridge yet");
    engine.handle(connected(), clock.at(1));
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(1),
    );
    assert_eq!(gps(&engine), None, "the bridge has gone");
}

/// An engine whose chain holds the bridge's receiver only, as under `--sim`, connected.
fn bridge_only_engine(clock: &Clock) -> FleetEngine {
    let position = PositionChain::empty().with_bridge_gps(Gps::bridged(DEFAULT_MAX_AGE));
    let mut engine = engine(EngineConfig { position, ..Default::default() }, clock);
    engine.handle(connected(), clock.at(1));
    engine
}

/// The P4's report that no receiver answers on its GPS port.
fn no_gps_receiver() -> Event {
    let message = LogStr::try_from(NO_GPS_RECEIVER).expect("the text fits a LogStr");
    Event::Link(LinkEvent::Message(BridgeToHost::Error { message }))
}

/// The GPS status the snapshot at `now` reports.
fn shown_gps_status(engine: &FleetEngine, now: Now) -> GpsStatus {
    engine.snapshot(now, StoreStats::default()).gps.expect("a receiver").status
}

#[test]
fn engine_snapshot_shows_connecting_when_bridge_has_not_reported_its_receiver() {
    let clock = Clock::new();
    let engine = bridge_only_engine(&clock);
    assert_eq!(shown_gps_status(&engine, clock.at(2)), GpsStatus::Connecting);
}

#[test]
fn engine_snapshot_shows_no_receiver_when_bridge_reports_none_and_chain_has_no_host_receiver() {
    let clock = Clock::new();
    let mut engine = bridge_only_engine(&clock);
    engine.handle(no_gps_receiver(), clock.at(1));
    let snapshot = engine.snapshot(clock.at(2), StoreStats::default());
    let gps = snapshot.gps.expect("a receiver");
    assert_eq!(gps.status, GpsStatus::NoReceiver);
    assert!(gps.via_bridge, "the bridge's receiver, as `g` names it");
}

#[test]
fn engine_snapshot_shows_no_receiver_when_bridge_reports_its_gps_bus_refused() {
    let clock = Clock::new();
    let mut engine = bridge_only_engine(&clock);
    let message = LogStr::try_from(GPS_BUS_REFUSED).expect("the text fits a LogStr");
    engine.handle(Event::Link(LinkEvent::Message(BridgeToHost::Error { message })), clock.at(1));
    assert_eq!(shown_gps_status(&engine, clock.at(2)), GpsStatus::NoReceiver);
}

#[test]
fn engine_snapshot_shows_connecting_when_bridge_says_its_receiver_answered() {
    let clock = Clock::new();
    let mut engine = bridge_only_engine(&clock);
    engine.handle(no_gps_receiver(), clock.at(1));
    let message = LogStr::try_from(GPS_RECEIVER_ANSWERED).expect("the text fits a LogStr");
    engine.handle(Event::Link(LinkEvent::Message(BridgeToHost::Log { message })), clock.at(2));
    assert_eq!(shown_gps_status(&engine, clock.at(2)), GpsStatus::Connecting);
}

#[test]
fn engine_snapshot_shows_connecting_when_bridge_reconnects_after_reporting_none() {
    // A new bridge life says again whether it has a receiver, right after its `Ready`.
    let clock = Clock::new();
    let mut engine = bridge_only_engine(&clock);
    engine.handle(no_gps_receiver(), clock.at(1));
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(2),
    );
    engine.handle(connected(), clock.at(3));
    assert_eq!(shown_gps_status(&engine, clock.at(3)), GpsStatus::Connecting);
}

#[test]
fn engine_records_gps_setting_when_bridge_is_connected() {
    // `run` builds the bridge's receiver for every capture, so its presence in the chain alone
    // says nothing about whether a receiver is being read.
    let clock = Clock::new();
    let position = PositionChain::empty().with_bridge_gps(Gps::bridged(DEFAULT_MAX_AGE));
    let mut engine = engine(EngineConfig { position, ..Default::default() }, &clock);
    assert!(!engine.capture_settings().gps, "no bridge yet");
    engine.handle(connected(), clock.at(2));
    assert!(engine.capture_settings().gps);
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(3),
    );
    assert!(!engine.capture_settings().gps);
}

#[test]
fn engine_records_gps_setting_when_chain_has_host_receiver() {
    let clock = Clock::new();
    let position = PositionChain::empty().with_gps(Gps::detached());
    let engine = engine(EngineConfig { position, ..Default::default() }, &clock);
    assert!(engine.capture_settings().gps);
}

#[test]
fn engine_records_no_gps_setting_when_chain_has_no_receiver() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(1));
    assert!(!engine.capture_settings().gps);
}

#[test]
fn engine_skips_status_poll_when_link_down() {
    // Every poll at a closed link is a command queued to be thrown away.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(connected(), clock.at(1));
    engine.handle(Event::Link(LinkEvent::Disconnected { reason: "cable".to_owned() }), clock.at(2));

    assert!(engine.handle(Event::Tick, clock.at(60)).bulk.is_empty());
}

#[test]
fn engine_drops_oldest_tail_entry_when_tail_full() {
    let clock = Clock::new();
    let config = EngineConfig { tail_len: 4, ..Default::default() };
    let mut engine = engine(config, &clock);
    introduce(&mut engine, &clock, NODE);

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
fn engine_renders_tail_ssid_as_text_when_ssid_cloaked_or_has_nul() {
    // An interior NUL is not padding: it survives the trim, and a terminal handed
    // one silently shows a shorter name than the one on the air.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);

    engine.handle(named_observation(NODE, "AA:BB:CC:DD:EE:01", -60, &[0u8; 8]), clock.at(1));
    engine.handle(named_observation(NODE, "AA:BB:CC:DD:EE:02", -60, b"a\0b"), clock.at(2));

    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    assert_eq!(snapshot.tail[0].ssid, "", "nothing but padding is a hidden network");
    assert_eq!(snapshot.tail[1].ssid, "a\u{FFFD}b");
}

#[test]
fn engine_attaches_position_when_sighting_arrives() {
    let clock = Clock::new();
    let config = EngineConfig {
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0)),
        ..Default::default()
    };
    let mut engine = engine(config, &clock);
    introduce(&mut engine, &clock, NODE);

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
fn engine_switches_to_gps_when_fix_arrives() {
    // The reason the source is a column and not a capture-wide setting.
    let clock = Clock::new();
    let gps = Gps::detached();
    let config = EngineConfig {
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0)).with_gps(gps.clone()),
        ..Default::default()
    };
    let mut engine = engine(config, &clock);
    introduce(&mut engine, &clock, NODE);

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
fn engine_falls_back_to_static_when_gps_fix_stale() {
    // Not "keeps the last fix forever": at driving speed a minute-old position is
    // a different street.
    let clock = Clock::new();
    let gps = Gps::detached();
    let config = EngineConfig {
        position: PositionChain::fixed(37.7749, -122.4194, Some(16.0)).with_gps(gps.clone()),
        ..Default::default()
    };
    let mut engine = engine(config, &clock);
    introduce(&mut engine, &clock, NODE);
    gps.feed(GGA, clock.at(10).unix_ms);

    let fresh = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:03", -60), clock.at(12));
    let fresh = observed(&fresh);
    assert_eq!(fresh.fix.source, PositionSource::Gps);

    let stale = engine.handle(observation(NODE, "AA:BB:CC:DD:EE:04", -60), clock.at(60));
    let stale = observed(&stale);
    assert_eq!(stale.fix.source, PositionSource::Static);
    assert_eq!(stale.fix.lat, Some(37.7749));

    // And the snapshot says the same thing, so header and rows cannot disagree.
    let snapshot = engine.snapshot(clock.at(60), StoreStats::default());
    assert_eq!(snapshot.position.source, PositionSource::Static);
    assert!(snapshot.gps.is_some(), "the receiver is still configured, just not believed");
}

#[test]
fn engine_records_raw_frame_only_when_record_raw() {
    let clock = Clock::new();
    let mut off = engine(EngineConfig::default(), &clock);
    let mut on = engine(EngineConfig { record_raw: true, ..Default::default() }, &clock);

    let raw = |batch: &wartui_core::ActionBatch| -> Vec<wartui_core::record::RawFrame> {
        batch
            .records
            .iter()
            .filter_map(|r| match r {
                Record::Raw(raw) => Some(raw.clone()),
                _ => None,
            })
            .collect()
    };
    let body = beat_msg(NODE, 1).encode();
    assert!(raw(&off.handle(heartbeat(NODE, 1), clock.at(1))).is_empty());
    let kept = raw(&on.handle(heartbeat(NODE, 1), clock.at(1)));
    assert_eq!(
        kept,
        [wartui_core::record::RawFrame {
            rx_at_ms: clock.at(1).unix_ms,
            slot: slot_of(NODE),
            kind: UpType::Heartbeat.as_u8(),
            bytes: body.to_vec(),
        }],
        "the record as it arrived, with the slot and type it came under"
    );
}

#[test]
fn engine_snapshot_orders_nodes_by_mac_when_heard_out_of_order() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(heartbeat(NODE, 1), clock.at(2));

    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    let macs: Vec<Mac> = snapshot.nodes.iter().map(|n| n.state.mac).collect();
    assert_eq!(macs, vec![NODE, OTHER]);
    assert_eq!(snapshot.alive, 2);
}

#[test]
fn engine_counts_bridge_drops_since_attach_when_status_arrives() {
    // A bridge left powered with nothing attached drops everything it hears, so
    // its own counter is dominated by history. What matters is *this* capture.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let status = |dropped_tx| {
        Event::Link(LinkEvent::Message(BridgeToHost::Status {
            rx_count: 1363,
            dropped_tx,
            uptime_ms: 3_240_000,
            rx_oversize: 0,
            queues: Default::default(),
            radio_link: RadioLinkStats::default(),
            host_uart_errors: 0,
            fan: None,
            pool: ChannelPool::All,
            present: ALL_PRESENT,
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

    // The bridge restarted and began again from zero, so the old baseline would
    // underflow or hide real losses.
    engine.handle(status(2), clock.at(8));
    assert_eq!(seen(&engine).dropped_since_attach, 0);
    engine.handle(status(9), clock.at(9));
    assert_eq!(seen(&engine).dropped_since_attach, 7);
}

#[test]
fn engine_records_bridge_status_when_reply_arrives() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let queues: QueueStats =
        [QueueStat { queue: QueueId::HostBulk, capacity: 262_144, high_water: 1_024, dropped: 5 }]
            .into_iter()
            .collect();
    let radio_link = RadioLinkStats {
        packets: 48_210,
        crc_fail: 2,
        bad_magic: 0,
        seq_gap: 0,
        uart_errors: 1,
        radio_crc_fail: 0,
        radio_bad_magic: 0,
        radio_seq_gap: 0,
        radio_uart_errors: 0,
    };
    let fan = Some(FanStats { duty: FanDuty::new(40).expect("in range"), rpm: Some(1830) });
    let status = |dropped_tx, uptime_ms, queues: &QueueStats, radio_link, fan| {
        Event::Link(LinkEvent::Message(BridgeToHost::Status {
            rx_count: 1363,
            dropped_tx,
            uptime_ms,
            rx_oversize: 17,
            queues: queues.clone(),
            radio_link,
            host_uart_errors: 0,
            fan,
            pool: ChannelPool::All,
            present: ALL_PRESENT,
        }))
    };
    let statuses = |batch: &ActionBatch| -> Vec<BridgeStatusSeen> {
        batch
            .records
            .iter()
            .filter_map(|r| match r {
                Record::BridgeStatus(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };

    engine.handle(connected(), clock.at(1));
    let first = engine.handle(status(1300, 3_240_000, &queues, radio_link, fan), clock.at(2));
    // Lower than before: the bridge rebooted, and the row keeps what it said.
    let rebooted = RadioLinkStats::default();
    let second = engine.handle(status(2, 4_000, &QueueStats::new(), rebooted, None), clock.at(7));

    assert_eq!(
        statuses(&first),
        vec![BridgeStatusSeen {
            rx_at_ms: clock.at(2).unix_ms,
            rx_count: 1363,
            dropped_tx: 1300,
            uptime_ms: 3_240_000,
            host_frames: 0,
            rx_oversize: 17,
            queues,
            radio_link,
            host_uart_errors: 0,
            fan,
            slots_present: ALL_PRESENT,
        }]
    );
    assert_eq!(
        statuses(&second),
        vec![BridgeStatusSeen {
            rx_at_ms: clock.at(7).unix_ms,
            rx_count: 1363,
            dropped_tx: 2,
            uptime_ms: 4_000,
            host_frames: 0,
            rx_oversize: 17,
            queues: QueueStats::new(),
            radio_link: rebooted,
            host_uart_errors: 0,
            fan: None,
            slots_present: ALL_PRESENT,
        }]
    );
    let shown = engine.snapshot(clock.at(7), StoreStats::default()).bridge_status;
    assert_eq!(shown.and_then(|status| status.fan), None, "the latest reply's fan");
}

#[test]
fn engine_records_host_frames_when_status_reply_arrives() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(1));
    engine.handle(heartbeat(NODE, 1), clock.at(2));
    engine.handle(stray(0), clock.at(2));
    engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(3));

    let batch = engine.handle(
        Event::Link(LinkEvent::Message(BridgeToHost::Status {
            rx_count: 10,
            dropped_tx: 0,
            uptime_ms: 9_000,
            rx_oversize: 0,
            queues: Default::default(),
            radio_link: RadioLinkStats::default(),
            host_uart_errors: 0,
            fan: None,
            pool: ChannelPool::All,
            present: ALL_PRESENT,
        })),
        clock.at(4),
    );
    let host_frames: Vec<u64> = batch
        .records
        .iter()
        .filter_map(|r| match r {
            Record::BridgeStatus(s) => Some(s.host_frames),
            _ => None,
        })
        .collect();
    assert_eq!(host_frames, vec![3], "every node record read off the link, the stray included");
    assert_eq!(counters(&engine).frames, 3);
}

/// The one host row in a batch.
fn host_status(batch: &ActionBatch) -> HostStatus {
    let rows: Vec<HostStatus> = batch
        .records
        .iter()
        .filter_map(|r| match r {
            Record::HostStatus(h) => Some(*h),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "exactly one host row");
    rows[0]
}

#[test]
fn engine_records_host_status_when_host_sample_arrives() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    introduce(&mut engine, &clock, NODE);
    let msg = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -60,
        security: Security::Open,
        ssid: b"",
        ext: &[],
    };
    let frame = batch(1, &[msg]);
    engine.handle(sightings_at(NODE, &frame, 0), clock.at(1));
    engine.handle(sightings_at(NODE, &frame, 4_000), clock.at(1));
    engine.handle(Event::Link(LinkEvent::Garbled(LinkError::Corrupt)), clock.at(1));

    let sample = HostSample {
        store: StoreStats { written: 40, dropped: 2 },
        peaks: StorePeaks { queue: 17, commit_us: 5_300 },
        health: Health {
            throttled: Some(0x50005),
            soc_temp_mc: Some(61_234),
            battery_mv: Some(4_185),
            battery_ma: Some(-21),
        },
    };
    let row = host_status(&engine.handle(Event::HostSample(sample), clock.at(5)));
    assert_eq!(
        row,
        HostStatus {
            at_ms: clock.at(5).unix_ms,
            frames: 3,
            duplicate_batches: 1,
            garbled: 1,
            store_written: 40,
            store_dropped: 2,
            store_queue_peak: 17,
            store_commit_peak_us: 5_300,
            throttled: Some(0x50005),
            soc_temp_mc: Some(61_234),
            battery_mv: Some(4_185),
            battery_ma: Some(-21),
            ..row
        }
    );
    assert_eq!((row.undecodable, row.incompatible), (0, 0));
}

#[test]
fn engine_reports_lag_peak_once_when_backlog_drains() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    // Live: a second on both clocks.
    engine.handle(
        beat_at(NODE, 1, 0, Capabilities::here(Board::Esp32c5), 1_000_000),
        clock.at_ms(1_000),
    );
    // Then three seconds of bridge time inside 3 ms of host time.
    for i in 1..=3u32 {
        engine.handle(
            beat_at(NODE, 1 + i, 0, Capabilities::here(Board::Esp32c5), 1_000_000 + i * 1_000_000),
            clock.at_ms(1_000 + u64::from(i)),
        );
    }
    // Caught up again.
    engine.handle(
        beat_at(NODE, 5, 0, Capabilities::here(Board::Esp32c5), 4_100_000),
        clock.at_ms(2_000),
    );

    let sample = || Event::HostSample(HostSample::default());
    let first = host_status(&engine.handle(sample(), clock.at(3)));
    assert_eq!(first.lag_peak_us, 3_000_000 - 3_000, "three seconds ahead, less 3 ms waited");
    let second = host_status(&engine.handle(sample(), clock.at(8)));
    assert_eq!(second.lag_peak_us, 0, "the peak starts again with each row");
}

#[test]
fn engine_reports_no_lag_peak_when_only_connected() {
    // A connection starts caught up, and nothing measured since says otherwise.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(0));
    let row = host_status(&engine.handle(Event::HostSample(HostSample::default()), clock.at(5)));
    assert_eq!(row.lag_peak_us, 0);

    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(6),
    );
    engine.handle(connected(), clock.at(7));
    // The first record after a connect has nothing to be timed against.
    engine.handle(stray(0), clock.at(7));
    let row = host_status(&engine.handle(Event::HostSample(HostSample::default()), clock.at(10)));
    assert_eq!(row.lag_peak_us, 0);
}

// ---------------------------------------------------------------------------
// Phase 4: assignment
// ---------------------------------------------------------------------------

#[test]
fn engine_sends_assignment_only_when_node_heartbeats() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, alone) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    assert_eq!(alone.epoch, 1, "the first epoch of a fresh database");
    engine.handle(delivered(id), clock.at(1));

    // A second node joins, so the pool is re-cut and the first node owes a
    // narrower share. A node is answered off its own heartbeat, so nothing goes
    // to it on somebody else's.
    let joined = engine.handle(heartbeat(OTHER, 1), clock.at(2));
    let (_, dst, _) = sent_admin(&joined);
    assert_eq!(dst, OTHER, "only the node that heartbeated is written to");
    assert!(engine.nodes().next().expect("the first node").dirty, "the other is owed one");

    let opened = engine.handle(heartbeat(NODE, 2), clock.at(6));
    let (_, dst, admin) = sent_admin(&opened);
    assert_eq!(dst, NODE);
    assert!(
        admin.channels.len() < alone.channels.len(),
        "its share narrowed now it shares the pool"
    );
    assert_eq!(counters(&engine).admin_sent, 3);
}

#[test]
fn engine_addresses_assignment_to_slot_when_heartbeat_arrives_from_it() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let slot = Slot::new(1, 4).expect("B5");

    let batch = engine.handle(
        from_slot(slot, UpType::Heartbeat.as_u8(), &beat_msg(NODE, 1).encode(), 0),
        clock.at(1),
    );

    // Urgent, since it answers a heartbeat, and to the slot that heartbeat came from.
    assert!(batch.bulk.is_empty());
    let sent = to_nodes(&batch);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (_, to, kind, body) = &sent[0];
    assert_eq!((*to, *kind), (slot, DownType::Assign.as_u8()));
    let assign = Assign::decode(body).expect("a valid assignment");
    assert_eq!(assign.channels, ChannelPool::default().channels());
    assert_eq!(engine.nodes().next().expect("the node").slot, Some(slot));
}

#[test]
fn engine_withholds_assignment_when_slot_heard_only_by_sightings() {
    // Until its heartbeat, nothing says who sits in the slot or what it runs.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(observation(NODE, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));

    let ticked = engine.handle(Event::Tick, clock.at(2));
    assert!(ticked.urgent.is_empty());
    assert_eq!(counters(&engine).admin_sent, 0);
    assert_eq!(engine.nodes().count(), 0, "and nothing is filed to queue one against");
    assert!(engine.snapshot(clock.at(2), StoreStats::default()).plan.is_none());
}

#[test]
fn engine_leaves_slot_out_of_plan_when_it_sends_only_sightings() {
    // The failure this exists to prevent, in the smallest form that shows it: two
    // nodes plus a slot no heartbeat has named must partition the pool two ways, not
    // three. Three would leave a third of it assigned to a node nothing confirmed.
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(observation(THIRD, "AA:BB:CC:DD:EE:FF", -60), clock.at(1));
    engine.handle(Event::Tick, clock.at(2));

    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    let plan = snapshot.plan.expect("a plan");
    assert_eq!(plan.node_count(), 2, "the unnamed slot is not one of the two");
    assert_eq!(snapshot.alive, 2, "and is not in the table to be counted alive");
    assert_eq!(counters(&engine).unknown_slot, 1);
    assert_eq!(snapshot.assignable, 2);

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
fn engine_holds_ble_for_named_node_when_it_has_not_heartbeated() {
    // Nothing is ever removed from the node table, so an absent MAC has never been
    // heard rather than having gone away. Reading absence as departure took the
    // scan back before the node's first heartbeat could arrive.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(Event::Command(Command::AssignBle { mac: Some(NODE) }), clock.at(1));
    for tick in 2..12 {
        engine.handle(Event::Tick, clock.at(tick));
    }
    assert_eq!(
        engine.snapshot(clock.at(12), StoreStats::default()).ble_node,
        Some(NODE),
        "ten ticks with no word from the node change nothing"
    );

    // A batch from its slot is not arrival either: no heartbeat has named the
    // slot yet, which across a restart can last a whole sweep.
    engine.handle(observation(NODE, "aa:bb:cc:dd:ee:ff", -50), clock.at(12));
    engine.handle(Event::Tick, clock.at(12));
    assert_eq!(
        engine.snapshot(clock.at(12), StoreStats::default()).ble_node,
        Some(NODE),
        "a slot that has only sent sightings has not gone quiet — it has not spoken yet"
    );

    // And when it does turn up, the flag is on the first assignment the planner
    // sends it: nothing else had to be asked for.
    let (_, dst, admin) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(13)));
    assert_eq!(dst, NODE);
    assert!(admin.scan_ble(), "the scan asked for before it arrived is the scan it is given");
}

#[test]
fn engine_confirms_assignment_when_bridge_delivers_it() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, admin) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // Only the bridge's `Delivered` clears the dirty flag.
    let batch = engine.handle(delivered(id), clock.at(6));
    let node = engine.nodes().next().expect("the node");
    assert!(!node.dirty, "delivered, so there is nothing left to deliver");
    assert_eq!(node.confirmed.expect("confirmed").channels, ChannelPool::Us.channels());
    assert_eq!(counters(&engine).admin_acked, 1);

    let Some(Record::Assignment(row)) = batch.records.first() else {
        panic!("every attempt is written down")
    };
    assert_eq!(row.outcome, AdminOutcome::Acked);
    assert_eq!(row.channels, ChannelPool::Us.channels());
    assert!(!row.ble);

    // And a later heartbeat, reporting the epoch it adopted, sends nothing:
    // there is nothing left to say.
    assert!(engine.handle(heartbeat_holding(NODE, 3, admin.epoch), clock.at(10)).urgent.is_empty());
}

#[test]
fn engine_sends_one_assignment_when_dirty_node_beats_twice_unanswered() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, ..) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));

    // The bus is still trying, so the next heartbeat sends and counts nothing.
    let second = engine.handle(heartbeat(NODE, 2), clock.at(2));
    assert!(admins(&second).is_empty(), "one assignment in flight per node");
    let node = engine.nodes().next().expect("the node");
    assert!(node.dirty);
    assert_eq!(node.admin_attempts, 1);
    assert_eq!(counters(&engine).admin_sent, 1);

    // Once the bridge answers, the next heartbeat sends again.
    engine.handle(undelivered(id, Undelivered::Timeout), clock.at(3));
    let (retry, ..) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(3)));
    assert_ne!(retry, id);
}

#[test]
fn engine_resends_assignment_when_unanswered_past_admin_timeout() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at_ms(1_000)));
    let timeout = u64::try_from(EngineConfig::default().admin_timeout.as_millis()).expect("fits");

    let early = engine.handle(heartbeat(NODE, 2), clock.at_ms(1_000 + timeout - 1));
    assert!(admins(&early).is_empty(), "still in flight");
    // Past the timeout the bridge's answer is taken as lost.
    sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at_ms(1_000 + timeout)));
}

#[test]
fn engine_retries_assignment_when_undelivered() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // The node acknowledged none of the bus's tries. A miss costs one heartbeat.
    let batch = engine.handle(undelivered(id, Undelivered::Timeout), clock.at(6));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Unacked);
    assert_eq!(row.latency_us, None, "nothing was delivered to time");
    assert_eq!(row.delivered_at_ms, Some(clock.at(6).unix_ms), "but the bridge did answer");
    assert!(engine.nodes().next().expect("the node").dirty, "still owed");

    let (_, _, retry) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(10)));
    assert_eq!(
        retry.epoch, first.epoch,
        "a retry of the same assignment keeps its epoch; the node never saw it"
    );
    assert_eq!(counters(&engine).admin_failed, 1);
}

#[test]
fn engine_records_silent_when_bridge_never_answers() {
    let clock = Clock::new();
    let config = EngineConfig { admin_timeout: Duration::from_secs(2), ..Default::default() };
    let mut engine = engine(config, &clock);
    sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(6)));

    assert!(engine.handle(Event::Tick, clock.at(7)).records.is_empty(), "still in flight");

    let batch = engine.handle(Event::Tick, clock.at(9));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    // Distinct from `Unacked`, where the bridge did report back. Here the bridge
    // itself went quiet, and the difference points at different hardware.
    assert_eq!(row.outcome, AdminOutcome::Silent);
    assert_eq!(row.latency_us, None);
    // Nothing arrived, so there is no delivery time to stamp.
    assert_eq!(row.delivered_at_ms, None);
    assert!(engine.nodes().next().expect("the node").dirty);
}

#[test]
fn engine_keeps_ack_when_older_attempt_times_out() {
    let clock = Clock::new();
    let config = EngineConfig { admin_timeout: Duration::from_secs(2), ..Default::default() };
    let mut engine = engine_on(ChannelPool::Us, config, &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    // The first attempt's answer is lost on the way back — a garbled USB frame.
    let (first, ..) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(3)));
    // So the node is still dirty on its next heartbeat. That one is past the
    // timeout but ahead of the tick that expires the first, so the retry goes out,
    // under the same epoch, and this one is acknowledged.
    let (second, ..) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(5)));
    assert_ne!(first, second);
    engine.handle(delivered(second), clock.at(5));

    let node = engine.nodes().next().expect("the node").clone();
    assert_eq!(node.confirmed.expect("confirmed").channels, ChannelPool::Us.channels());

    // Now the stranded first attempt times out. It earns its row, but the node is
    // demonstrably holding the range.
    let batch = engine.handle(Event::Tick, clock.at(5));
    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Silent);

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.last_outcome, Some(AdminOutcome::Acked));
    assert_eq!(node.confirmed.expect("still confirmed").channels, ChannelPool::Us.channels());
    assert!(!node.dirty, "nothing is owed: the node acknowledged this epoch");
}

#[test]
fn engine_records_assignment_latency_when_delivered() {
    // Timed on this host's clock, from the heartbeat that sent it to the bridge's
    // `Delivered`: the bridge's stamps play no part.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at_ms(6_000)));
    let batch = engine.handle(delivered(id), clock.at_ms(6_004));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.latency_us, Some(4_000));
    assert_eq!(engine.nodes().next().expect("the node").last_latency_us, Some(4_000));
}

#[test]
fn engine_records_latency_when_delivery_is_slow() {
    // A node polled late is still a delivery, and the time it took is the figure.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));

    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at_ms(6_000)));
    let batch = engine.handle(delivered(id), clock.at_ms(7_500));

    let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
    assert_eq!(row.outcome, AdminOutcome::Acked);
    assert_eq!(row.latency_us, Some(1_500_000));
}

#[test]
fn engine_reissues_assignment_when_node_reboots() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 40), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 41), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));
    assert!(engine.nodes().next().expect("the node").confirmed.is_some());

    // The node rebooted. Its own version field went back to a boot value with it, and
    // since a node adopts on `!=` rather than `>`, re-sending the old epoch could
    // match what it now holds and be discarded — while still being acknowledged.
    let batch = engine.handle(heartbeat(NODE, 1), clock.at(10));
    let (_, _, reissued) = sent_admin(&batch);
    assert_ne!(reissued.epoch, first.epoch);
    assert_eq!(reissued.channels, first.channels, "and the share it was already holding");

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.reboots, 1);
    assert!(node.confirmed.is_none(), "what it held is no longer believed");
}

// ---------------------------------------------------------------------------
// Phase 4b: adoption — the heartbeat's own epoch, layered on top of the
// bus's delivery.
// ---------------------------------------------------------------------------

#[test]
fn engine_marks_adopted_when_heartbeat_reports_epoch() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));
    assert!(!engine.nodes().next().expect("the node").adopted(), "delivered, not yet adopted");

    engine.handle(heartbeat_holding(NODE, 3, first.epoch), clock.at(10));
    assert!(engine.nodes().next().expect("the node").adopted());
}

#[test]
fn engine_resends_when_delivered_epoch_not_adopted() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));

    // Delivered, but the node's own heartbeat says it holds something else —
    // a length or version mismatch, a dropped inbox, or a bug. Re-sent on this
    // heartbeat, and no epoch burned: the node still does not hold it either
    // way.
    let other = first.epoch.wrapping_add(1);
    let batch = engine.handle(heartbeat_holding(NODE, 3, other), clock.at(10));
    let (_, _, resent) = sent_admin(&batch);
    assert_eq!(resent.epoch, first.epoch, "no epoch burned on a retry");
    assert_eq!(resent.channels, first.channels);
    assert_eq!(counters(&engine).admin_unadopted, 1);
    assert_eq!(engine.nodes().next().expect("the node").unadopted, 1);
}

#[test]
fn engine_counts_unadopted_when_epoch_zero_after_first_delivery() {
    // The never-adopted case folded into the same fact: a `0` from a node that
    // has never once reported holding anything is still "not what was delivered".
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));

    let batch = engine.handle(heartbeat_holding(NODE, 3, 0), clock.at(10));
    let (_, _, resent) = sent_admin(&batch);
    assert_eq!(resent.epoch, first.epoch, "it still does not hold it either way");
    assert_eq!(counters(&engine).admin_unadopted, 1);

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.unadopted, 1);
    assert_eq!(node.reboots, 0, "never having adopted anything is not a reboot");
}

#[test]
fn engine_reissues_when_epoch_returns_to_zero() {
    // The counter keeps climbing; only the epoch says the node forgot
    // everything, which the counter check alone would miss.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));
    engine.handle(heartbeat_holding(NODE, 3, first.epoch), clock.at(10));
    assert!(engine.nodes().next().expect("the node").adopted());

    let batch = engine.handle(heartbeat_holding(NODE, 4, 0), clock.at(11));
    let (_, _, reissued) = sent_admin(&batch);
    assert_ne!(reissued.epoch, first.epoch, "a fresh epoch");
    assert_eq!(reissued.channels, first.channels, "the share it was already holding");
}

#[test]
fn engine_counts_reboot_when_epoch_returns_to_zero() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));
    engine.handle(heartbeat_holding(NODE, 3, first.epoch), clock.at(10));
    assert!(engine.nodes().next().expect("the node").adopted());

    // Owed a cleared ring too, so this reboot's reset can be told apart from
    // one that never ran.
    engine.handle(Event::Command(Command::ClearRing { mac: Some(NODE) }), clock.at(10));
    assert!(engine.nodes().next().expect("the node").clear_dedup_ring);

    engine.handle(heartbeat_holding(NODE, 4, 0), clock.at(11));

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.reboots, 1, "the epoch alone is enough to count it");
    assert!(node.confirmed.is_none(), "what it held is no longer believed");
    assert!(!node.clear_dedup_ring, "a node that just booted already holds an empty ring");
}

#[test]
fn engine_ignores_epoch_when_heartbeat_replayed() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));
    engine.handle(heartbeat_holding(NODE, 3, first.epoch), clock.at(10));
    assert!(engine.nodes().next().expect("the node").adopted());

    // A heartbeat replayed out of the bridge's backlog: its own clock jumps far
    // ahead of the host's, the tell that it is history. It reports
    // epoch 0 -- what a reboot would say -- but it is history, not now, and
    // must not be believed.
    let batch = engine.handle(
        beat_at(NODE, 4, 0, Capabilities::here(Board::Esp32c5), 1_000_000),
        clock.at_ms(10_002),
    );
    assert!(batch.urgent.is_empty(), "no assignment answers a replayed heartbeat");

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.reboots, 0, "a replayed heartbeat is not a reboot");
    assert!(node.adopted(), "held_epoch still says what the live heartbeat said");
    assert_eq!(counters(&engine).admin_unadopted, 0);
}

#[test]
fn engine_counts_one_reboot_when_replay_then_live_both_show_it() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));
    engine.handle(delivered(id), clock.at(6));
    engine.handle(heartbeat_holding(NODE, 3, first.epoch), clock.at(10));
    assert!(engine.nodes().next().expect("the node").adopted());

    // Replayed out of the backlog (a huge rx_us jump against a tiny host-time
    // one) and its counter is behind the one already seen: a reboot on its
    // own, and this alone must not also wait for an epoch to say so.
    engine.handle(
        beat_at(NODE, 1, 0, Capabilities::here(Board::Esp32c5), 1_000_000),
        clock.at_ms(10_002),
    );
    assert_eq!(engine.nodes().next().expect("the node").reboots, 1);

    // The next heartbeat is live and reports epoch 0 too -- the same reboot's
    // first live word on it, not a second reboot. `held_epoch` has to have
    // been cleared already by the replayed one above, or this looks like the
    // node forgetting a second, different epoch.
    engine.handle(
        beat_at(NODE, 2, 0, Capabilities::here(Board::Esp32c5), 1_050_000),
        clock.at_ms(10_102),
    );
    assert_eq!(engine.nodes().next().expect("the node").reboots, 1, "one reboot, not two");
}

#[test]
fn engine_records_unacked_and_retries_when_bridge_cannot_deliver() {
    // Every reason the bridge gives is the same outcome to the host: not delivered, so
    // still owed, and re-sent under the same epoch off the next heartbeat.
    for reason in [Undelivered::NoNode, Undelivered::QueueFull, Undelivered::Timeout] {
        let clock = Clock::new();
        let mut engine = engine(EngineConfig::default(), &clock);
        engine.handle(heartbeat(NODE, 1), clock.at(1));
        let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

        let batch = engine.handle(undelivered(id, reason), clock.at(6));

        let Some(Record::Assignment(row)) = batch.records.first() else { panic!("a row") };
        assert_eq!(row.outcome, AdminOutcome::Unacked, "{reason:?}");
        assert_eq!(row.latency_us, None, "{reason:?}");
        let node = engine.nodes().next().expect("the node");
        assert!(node.dirty, "{reason:?}: still owed");
        assert!(node.confirmed.is_none(), "{reason:?}");
        assert_eq!(node.last_outcome, Some(AdminOutcome::Unacked), "{reason:?}");
        assert_eq!((counters(&engine).admin_acked, counters(&engine).admin_failed), (0, 1));

        let (_, dst, retry) = sent_admin(&engine.handle(heartbeat(NODE, 3), clock.at(10)));
        assert_eq!(dst, NODE, "{reason:?}");
        assert_eq!(retry.epoch, first.epoch, "{reason:?}: the node never saw it");
    }
}

#[test]
fn engine_ignores_delivery_when_epoch_superseded() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    let (id, _, alone) = sent_admin(&engine.handle(heartbeat(NODE, 2), clock.at(6)));

    // A node joined while the first assignment was in flight, so the pool was re-cut
    // and the delivery that follows is for a share nobody holds any more.
    engine.handle(heartbeat(OTHER, 1), clock.at(6));
    engine.handle(delivered(id), clock.at(7));

    let node = engine.nodes().next().expect("the node");
    assert!(node.dirty, "the newer assignment is still owed");
    assert!(node.confirmed.is_none());
    let desired = node.desired.expect("desired");
    assert_ne!(desired.channels, alone.channels, "and it is not the share that was delivered");
}

// ---------------------------------------------------------------------------
// Partitioning: the engine holding the whole fleet on a cut of the pool, which
// is what "replaces the core" means rather than "watches it".
// ---------------------------------------------------------------------------

/// The `n`th node of a fake fleet. Ordered by MAC, which is the order the
/// planner numbers them in.
fn peer(n: u8) -> Mac {
    [0x02, 0x00, 0x5E, 0x10, 0x57, n]
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
fn engine_partitions_whole_pool_when_nodes_join() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }

    // Covering the pool exactly is coverage and disjointness in one assertion.
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));

    // And every node holds some of both bands, which is what dealing buys over
    // block-splitting.
    for node in engine.nodes() {
        let set = node.desired.expect("every member of the plan has channels").channels;
        for band in ChannelPool::Us.runs() {
            assert!(
                (band.start..=band.end).any(|idx| set.contains(idx)),
                "node got nothing from {band:?}"
            );
        }
    }
}

#[test]
fn engine_repartitions_when_membership_changes() {
    // The difference between wartui and a monitor, and the only author of an
    // assignment there is: no command reaches this, and no configuration turns
    // it off.
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    for n in 0..3 {
        let batch = engine.handle(heartbeat(peer(n), 1), clock.at(1));
        assert!(!batch.urgent.is_empty(), "a node's share answers its own heartbeat");
    }
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));

    // A fourth node joining is answered the same way, however long the capture
    // has been running.
    engine.handle(heartbeat(peer(3), 1), clock.at(3));
    let joined = engine.nodes().find(|node| node.mac == peer(3)).expect("the new node");
    assert!(joined.desired.is_some(), "and it is dealt a share on arrival");
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));
}

#[test]
fn engine_reissues_every_share_when_node_joins() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    // A node's own first heartbeat is what admits it, and its share answers that
    // heartbeat — not the next one.
    let (id, dst, first) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    assert_eq!(dst, peer(0));
    let plan_of = |engine: &FleetEngine, at| {
        engine.snapshot(clock.at(at), StoreStats::default()).plan.expect("a plan").node_count()
    };
    assert_eq!(plan_of(&engine, 1), 1);
    engine.handle(delivered(id), clock.at(1));

    let (_, _, second) = sent_admin(&engine.handle(heartbeat(peer(1), 1), clock.at(2)));
    assert_eq!(plan_of(&engine, 2), 2, "the fleet the range was cut for");
    assert!(second.channels.len() < first.channels.len(), "narrower now it shares the pool");

    // The settled node is owed a new range too: the plan just changed shape.
    let settled = engine.nodes().next().expect("the first node");
    assert!(settled.dirty, "re-issued, though it was acknowledged a moment ago");
    assert_ne!(
        settled.desired.expect("a new range").channels,
        settled.confirmed.expect("what it still holds").channels
    );

    let (_, dst, third) = sent_admin(&engine.handle(heartbeat(peer(0), 2), clock.at(6)));
    assert_eq!(dst, peer(0), "and it answers that node's own next heartbeat");
    assert_ne!(third.epoch, first.epoch, "under a fresh epoch");
    assert_eq!(counters(&engine).replans, 2);
}

#[test]
fn engine_redeals_share_when_node_falls_silent() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(heartbeat(peer(1), 1), clock.at(2));
    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));

    // Topology is driven by heartbeats alone.
    engine.handle(heartbeat(peer(1), 2), clock.at(70));

    let departed = engine.nodes().next().expect("the node that went quiet");
    assert!(departed.desired.is_none(), "nothing is owed to a node that cannot receive it");
    assert!(!departed.dirty);
    let survivor = engine.nodes().nth(1).expect("the node still beating");
    assert_eq!(
        covered(&[survivor.desired.expect("a range").channels]),
        pool_indices(ChannelPool::Us),
        "alone now, it holds the whole pool"
    );
}

#[test]
fn engine_gives_whole_pool_once_when_fleet_is_one_node() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);

    // Nothing here rotates: one contiguous range cannot say the US pool's two runs,
    // but a `ChannelSet` can, so a lone node is dealt the whole pool in one assignment.
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    assert_eq!(first.channels, ChannelPool::Us.channels());
    assert_eq!(first.channels.len(), 36, "eleven 2.4 GHz channels and twenty-five 5 GHz");
    engine.handle(delivered(id), clock.at(1));

    // And nothing re-issues it. The plan has no timer and ticks drive no dwell, so
    // a plan still holding after several minutes of them is the whole claim. The
    // node keeps heartbeating what it holds, so it stays in the fleet.
    for minute in 1..=5 {
        let at = clock.at(60 * minute);
        let beat = u32::try_from(minute).expect("fits") + 1;
        engine.handle(heartbeat_holding(peer(0), beat, first.epoch), at);
        let batch = engine.handle(Event::Tick, at);
        assert!(batch.urgent.is_empty(), "nothing is owed at minute {minute}");
    }
    assert_eq!(counters(&engine).replans, 1, "one plan, once");
}

#[test]
fn engine_recuts_both_ends_when_ble_scan_moves() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    // Both join, then both settle: the second node's arrival re-cuts the pool,
    // so the shares that stick are the ones sent on the second heartbeat.
    for n in 0..2 {
        let first = engine.handle(heartbeat(peer(n), 1), clock.at(1));
        deliver_all(&mut engine, &first, clock.at(1));
    }
    for n in 0..2 {
        let (id, ..) = sent_admin(&engine.handle(heartbeat(peer(n), 2), clock.at(2)));
        engine.handle(delivered(id), clock.at(2));
    }
    // Nobody scans Bluetooth unless asked — the cost is measured and real, so it
    // is not a fleet-wide default.
    assert!(engine.nodes().all(|node| !node.confirmed.expect("planned").ble));
    assert_eq!(engine.snapshot(clock.at(2), StoreStats::default()).ble_node, None);

    let whole_pool = ChannelPool::default().channels();
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(3));
    // Bluetooth is the whole job, so the scan does not ride on the share — it takes
    // its place. One epoch carries both.
    let (id, dst, admin) = sent_admin(&engine.handle(heartbeat(peer(0), 3), clock.at(4)));
    assert_eq!(dst, peer(0));
    assert!(admin.scan_ble(), "the flag arrives in that node's own next assignment");
    assert!(admin.channels.is_empty(), "and it sniffs no Wi-Fi at all");
    assert_ne!(admin.epoch, 0, "under an epoch the node will adopt");
    engine.handle(delivered(id), clock.at(4));

    // And the pool is re-cut across what is left, which here is one node: giving the
    // scan away is a change to every other node's share, not just to that node's.
    let (id, dst, rest) = sent_admin(&engine.handle(heartbeat(peer(1), 3), clock.at(5)));
    assert_eq!(dst, peer(1));
    assert_eq!(rest.channels, whole_pool, "the only node still sniffing takes all of it");
    assert!(!rest.scan_ble());
    engine.handle(delivered(id), clock.at(5));

    // Moving it is two assignments: the node giving it up has a share coming back to it.
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(1)) }), clock.at(6));
    let (_, _, gave_up) = sent_admin(&engine.handle(heartbeat(peer(0), 4), clock.at(7)));
    assert!(!gave_up.scan_ble());
    assert_eq!(gave_up.channels, whole_pool, "and takes the whole pool with it");
    let (_, _, took_it) = sent_admin(&engine.handle(heartbeat(peer(1), 4), clock.at(8)));
    assert!(took_it.scan_ble());
    assert!(took_it.channels.is_empty());
}

#[test]
fn engine_sends_empty_channels_only_with_ble_flag_when_ble_moves_in_three_node_fleet() {
    // The one assignment that must never exist: a node told to scan nothing parks, and
    // this host goes on believing it is sweeping. Every path that can produce one
    // is in here — a join, the scan given, the scan moved, the holder rebooting,
    // and the holder going quiet — because the two halves are computed
    // separately at the wire and only agree by construction.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let mut seen_empty = 0;
    let check = |batch: &ActionBatch, seen_empty: &mut u32| {
        for admin in admins(batch) {
            assert_eq!(
                admin.channels.is_empty(),
                admin.scan_ble(),
                "an empty mask and the Bluetooth flag are the same fact"
            );
            if admin.channels.is_empty() {
                *seen_empty += 1;
            }
        }
    };

    // Each assignment is delivered, so the next heartbeat is free to send another.
    for n in 0..3 {
        let batch = engine.handle(heartbeat(peer(n), 1), clock.at(1));
        check(&batch, &mut seen_empty);
        deliver_all(&mut engine, &batch, clock.at(1));
    }
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(1)) }), clock.at(2));
    for n in 0..3 {
        let batch = engine.handle(heartbeat(peer(n), 2), clock.at(3));
        check(&batch, &mut seen_empty);
        deliver_all(&mut engine, &batch, clock.at(3));
    }
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(2)) }), clock.at(4));
    for n in 0..3 {
        let batch = engine.handle(heartbeat(peer(n), 3), clock.at(5));
        check(&batch, &mut seen_empty);
        deliver_all(&mut engine, &batch, clock.at(5));
    }
    // The holder rebooting, and then the fleet aging out around it — both leave
    // the scan in place rather than moving it, so the invariant still has to hold.
    check(&engine.handle(heartbeat(peer(2), 1), clock.at(6)), &mut seen_empty);
    check(&engine.handle(Event::Tick, clock.at(7)), &mut seen_empty);
    check(&engine.handle(Event::Tick, clock.at(200)), &mut seen_empty);

    assert!(seen_empty >= 2, "and the Bluetooth node really was sent one: {seen_empty}");
}

#[test]
fn engine_reports_whole_pool_unreachable_when_lone_node_scans_ble() {
    // The cost of making Bluetooth a whole node's job, stated: a fleet of one that
    // holds the scan sweeps nothing. A plan rather than a refusal, because refusing
    // would leave the node holding the share it already had — still sniffing Wi-Fi,
    // which is the opposite of what was asked — and the footer is what says so.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(3));
    let (id, _, admin) = sent_admin(&engine.handle(heartbeat(peer(0), 3), clock.at(4)));
    assert!(admin.scan_ble() && admin.channels.is_empty());
    engine.handle(delivered(id), clock.at(4));

    let snapshot = engine.snapshot(clock.at(4), StoreStats::default());
    let plan = snapshot.plan.expect("a fleet of one is still a fleet");
    assert_eq!(plan.node_count(), 1, "and the scanner is still its slot");
    assert_eq!(
        plan.unreachable(),
        ChannelPool::default().channels(),
        "every channel of the pool went into no share"
    );
}

/// Drive a fleet of `nodes` until every assignment has landed, answering each one with a
/// delivery.
fn settle_fleet(engine: &mut FleetEngine, clock: &Clock, nodes: u8, from: u64) {
    for beat in 0..6u32 {
        let at = clock.at(from + u64::from(beat));
        engine.handle(Event::Tick, at);
        for n in 0..nodes {
            let batch = engine.handle(heartbeat(peer(n), beat + 1), at);
            if let Some(HostToBridge::ToNode { id, .. }) = batch.urgent.first() {
                engine.handle(delivered(*id), at);
            }
        }
    }
}

#[test]
fn engine_returns_scanner_to_wifi_when_scan_taken_off() {
    // The Bluetooth node holds nothing to scan, so the scan coming off it has to deal it a
    // share. Otherwise it would go on holding the antenna while the fleet table showed a
    // node the host does not think is the holder, and `b` would report a withdrawal that
    // never happened.
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(2)) }), clock.at(2));
    settle_fleet(&mut engine, &clock, 3, 2);
    let scanner = engine.nodes().find(|node| node.mac == peer(2)).expect("here");
    let held = scanner.confirmed.expect("it took the scan");
    assert!(held.ble && held.channels.is_empty(), "it really holds the scan: {held:?}");

    engine.handle(Event::Command(Command::AssignBle { mac: None }), clock.at(20));
    settle_fleet(&mut engine, &clock, 3, 20);

    assert_eq!(engine.snapshot(clock.at(40), StoreStats::default()).ble_node, None);
    for node in engine.nodes() {
        let held = node.desired.or(node.confirmed).expect("in the plan");
        assert!(!held.ble, "{:02X?} is still scanning Bluetooth", node.mac);
        assert!(!held.channels.is_empty(), "{:02X?} was left with nothing to scan", node.mac);
    }
    let back = engine.nodes().find(|node| node.mac == peer(2)).expect("here");
    let held = back.desired.or(back.confirmed).expect("in the plan");
    let share = plan(ChannelPool::Us, 3).expect("a valid fleet").channels_for(2);
    assert_eq!(Some(held.channels), share, "its slot's share of the pool");
}

#[test]
fn engine_keeps_one_ble_scanner_when_scan_moves() {
    // The scan moved rather than withdrawn, so one node gives it up as another takes it.
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(2)) }), clock.at(2));
    settle_fleet(&mut engine, &clock, 3, 2);
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(20));
    settle_fleet(&mut engine, &clock, 3, 20);

    let scanning: Vec<Mac> = engine
        .nodes()
        .filter(|node| node.desired.or(node.confirmed).is_some_and(|held| held.ble))
        .map(|node| node.mac)
        .collect();
    assert_eq!(scanning, vec![peer(0)], "at most one node scans Bluetooth");
}

#[test]
fn engine_drops_ble_node_when_it_ages_out() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(heartbeat(peer(1), 1), clock.at(1));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(2));
    assert_eq!(engine.snapshot(clock.at(2), StoreStats::default()).ble_node, Some(peer(0)));

    // It ages out with the node, or the view would claim Bluetooth coverage
    // nothing is providing.
    engine.handle(heartbeat(peer(1), 2), clock.at(70));
    engine.handle(Event::Tick, clock.at(70));
    assert_eq!(engine.snapshot(clock.at(70), StoreStats::default()).ble_node, None);
}

#[test]
fn engine_sends_nothing_when_plan_settled() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    // Acknowledge each node's range, which takes the fleet to the state it
    // spends the whole capture in.
    let mut epochs = [0u8; 3];
    for n in 0..3 {
        let (id, _, admin) = sent_admin(&engine.handle(heartbeat(peer(n), 2), clock.at(5)));
        epochs[usize::from(n)] = admin.epoch;
        engine.handle(delivered(id), clock.at(5));
    }
    let settled = counters(&engine).admin_sent;

    // A node adopts on `!=`, so an epoch it already holds is acknowledged and
    // discarded — re-cutting an unchanged fleet would do that at heartbeat rate.
    // Each node's heartbeat reports the epoch it adopted, the same as a real one
    // would once its assignment landed.
    for beat in 3..6u32 {
        let at = clock.at(u64::from(10 + beat));
        engine.handle(Event::Tick, at);
        for n in 0..3 {
            let batch = engine.handle(heartbeat_holding(peer(n), beat, epochs[usize::from(n)]), at);
            assert!(batch.urgent.is_empty(), "nothing left to say");
        }
    }
    assert_eq!(counters(&engine).admin_sent, settled);
    assert_eq!(counters(&engine).replans, 3, "once per node that joined, and no more");
}

#[test]
fn engine_reissues_under_new_epoch_when_rebooted_node_rejoins() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (first, _, _) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(1)));
    engine.handle(delivered(first), clock.at(1));
    let (second, _, _) = sent_admin(&engine.handle(heartbeat(peer(1), 1), clock.at(2)));
    engine.handle(delivered(second), clock.at(2));
    let (recut, _, recut_admin) = sent_admin(&engine.handle(heartbeat(peer(0), 2), clock.at(3)));
    engine.handle(delivered(recut), clock.at(3));

    // It goes quiet long enough to leave the plan, then comes back to a re-cut
    // that gives it the channels it is already holding. Each heartbeat reports
    // the epoch it adopted, the same as a real node's would.
    engine.handle(heartbeat(peer(1), 2), clock.at(70));
    engine.handle(heartbeat_holding(peer(0), 3, recut_admin.epoch), clock.at(71));
    assert!(
        engine
            .handle(heartbeat_holding(peer(0), 4, recut_admin.epoch), clock.at(75))
            .urgent
            .is_empty(),
        "a node already scanning its share is not told so again"
    );

    // A reboot forgets the range and the node's own version byte with it, so the
    // share has to be said again under a fresh epoch — which needs wartui to
    // still know what the node was holding.
    let batch = engine.handle(heartbeat(peer(0), 1), clock.at(80));
    let (_, dst, admin) = sent_admin(&batch);
    assert_eq!(dst, peer(0));
    assert_eq!(
        admin.channels,
        plan(ChannelPool::Us, 2).expect("two nodes").channels_for(0).expect("assigned")
    );
}

/// The default topology timeout, in whole seconds: the tests about it count from this.
fn timeout_s() -> u64 {
    EngineConfig::default().topology_timeout.as_secs()
}

/// Whether the engine has marked `mac` as gone from the fleet.
fn departed(engine: &FleetEngine, mac: Mac) -> bool {
    engine.nodes().find(|node| node.mac == mac).expect("the node").departed
}

#[test]
fn engine_marks_node_departed_when_silent_past_topology_timeout() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    let t = timeout_s();
    engine.handle(Event::Tick, clock.at(t));
    assert!(!departed(&engine, NODE), "a second short of the timeout is inside it");
    engine.handle(Event::Tick, clock.at(t + 2));
    assert!(departed(&engine, NODE));

    // Its next heartbeat brings it back.
    engine.handle(heartbeat(NODE, 2), clock.at(t + 10));
    assert!(!departed(&engine, NODE));
}

/// The snapshot's row for `mac`.
fn view_of(engine: &FleetEngine, mac: Mac, now: Now) -> NodeView {
    let snapshot = engine.snapshot(now, StoreStats::default());
    snapshot.nodes.into_iter().find(|view| view.state.mac == mac).expect("the node")
}

#[test]
fn engine_deals_whole_pool_to_other_node_when_status_shows_slot_absent() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    let snapshot = engine.snapshot(clock.at(1), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 2);

    engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(2));

    // On that `Status`, with no tick between and the node's heartbeat well inside the timeout.
    let snapshot = engine.snapshot(clock.at(2), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 1);
    assert_eq!(snapshot.alive, 1);
    let other = engine.nodes().find(|n| n.mac == OTHER).expect("filed");
    assert_eq!(other.desired.expect("dealt").channels, ChannelPool::Us.channels());
    let gone = view_of(&engine, NODE, clock.at(2));
    assert!(!gone.alive && !gone.on_bus && !gone.assignable, "{gone:?}");
}

#[test]
fn engine_returns_node_to_plan_when_heartbeat_follows_absent_status() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(2));

    // Plugged back in: its next heartbeat is newer than the `Status` that missed it.
    engine.handle(heartbeat(NODE, 2), clock.at(3));

    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 2);
    let back = view_of(&engine, NODE, clock.at(3));
    assert!(back.alive && back.on_bus && back.assignable, "{back:?}");
}

#[test]
fn engine_keeps_node_out_of_plan_when_slot_returns_without_heartbeat() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(2));

    // Its slot answers again, but no heartbeat has said which node is there or what it holds: a
    // rebooting node, or another one pushed into the socket.
    engine.handle(pool_status(ChannelPool::Us), clock.at(7));
    let snapshot = engine.snapshot(clock.at(7), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 1);
    assert!(!view_of(&engine, NODE, clock.at(7)).on_bus);

    engine.handle(heartbeat(NODE, 2), clock.at(8));
    let snapshot = engine.snapshot(clock.at(8), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 2);
    assert!(view_of(&engine, NODE, clock.at(8)).on_bus);
}

#[test]
fn engine_releases_bluetooth_scan_when_status_shows_its_node_slot_absent() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(NODE) }), clock.at(1));
    engine.handle(Event::Tick, clock.at(1));
    assert_eq!(engine.snapshot(clock.at(1), StoreStats::default()).ble_node, Some(NODE));

    engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(2));

    // On that `Status`, with no tick between.
    let snapshot = engine.snapshot(clock.at(2), StoreStats::default());
    assert_eq!(snapshot.ble_node, None);
    let other = engine.nodes().find(|n| n.mac == OTHER).expect("filed");
    assert_eq!(other.desired.expect("dealt").channels, ChannelPool::Us.channels());
}

#[test]
fn engine_decides_membership_by_heartbeats_when_no_status_has_arrived() {
    let clock = Clock::new();
    let mut engine = FleetEngine::new(EngineConfig::default(), clock.at(0));
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(heartbeat(OTHER, 1), clock.at(1));

    let snapshot = engine.snapshot(clock.at(2), StoreStats::default());
    assert_eq!((snapshot.alive, snapshot.assignable), (2, 2));
    assert!(snapshot.nodes.iter().all(|view| view.on_bus), "nothing has said otherwise");
    // Past the timeout, heartbeats alone still decide.
    let snapshot = engine.snapshot(clock.at(1 + timeout_s()), StoreStats::default());
    assert_eq!(snapshot.alive, 0);
}

#[test]
fn engine_drops_present_node_from_plan_when_heartbeats_stop_for_topology_timeout() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    let t = timeout_s();
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    // A node that answers the bus but has stopped heartbeating: its slot stays present.
    for (counter, at) in (1..).zip((1..=t + 1).step_by(5)) {
        engine.handle(heartbeat(OTHER, counter), clock.at(at));
        engine.handle(pool_status(ChannelPool::Us), clock.at(at));
    }

    engine.handle(Event::Tick, clock.at(t));
    let snapshot = engine.snapshot(clock.at(t), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 2, "a second short is inside it");
    engine.handle(Event::Tick, clock.at(t + 1));
    let snapshot = engine.snapshot(clock.at(t + 1), StoreStats::default());
    assert_eq!(snapshot.plan.expect("a plan").node_count(), 1);
    let silent = view_of(&engine, NODE, clock.at(t + 1));
    assert!(!silent.alive && silent.on_bus, "{silent:?}");
}

#[test]
fn engine_ignores_earlier_slot_mask_when_link_reconnects() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(2));
    assert!(!view_of(&engine, NODE, clock.at(2)).on_bus);

    // The bridge may have restarted meanwhile, so its old mask says nothing about now.
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(3),
    );
    engine.handle(connected(), clock.at(4));
    let view = view_of(&engine, NODE, clock.at(5));
    assert!(view.on_bus && view.alive, "{view:?}");

    engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(6));
    assert!(!view_of(&engine, NODE, clock.at(6)).on_bus, "a new `Status` applies");
}

#[test]
fn engine_logs_off_bus_reason_when_status_shows_node_slot_absent() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));
    // Departures are logged once the link has been up a whole timeout.
    let t = timeout_s();
    engine.handle(heartbeat(NODE, 2), clock.at(t - 5));

    let lines = Logs::during(|| {
        engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(t + 1));
        engine.handle(Event::Tick, clock.at(t + 1));
    });

    let left: Vec<&String> = lines.iter().filter(|line| line.contains("node left")).collect();
    assert_eq!(left.len(), 1, "logged once: {lines:?}");
    assert!(left[0].contains("reason=\"off bus\""), "{left:?}");
    assert!(left[0].contains("silent_ms=6000"), "{left:?}");
}

#[test]
fn engine_logs_second_off_bus_departure_when_node_returns_by_heartbeat_and_drops_again() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let t = timeout_s();
    engine.handle(heartbeat(NODE, 1), clock.at(t - 5));

    let lines = Logs::during(|| {
        engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(t + 1));
        engine.handle(heartbeat(NODE, 2), clock.at(t + 5));
        engine.handle(slots_status(ChannelPool::Us, without(NODE)), clock.at(t + 10));
    });

    let named = |message: &str| lines.iter().filter(|line| line.contains(message)).count();
    assert_eq!(named("node left the fleet"), 2, "{lines:?}");
    assert_eq!(named("node returned"), 1, "{lines:?}");
    assert!(departed(&engine, NODE), "gone again");
}

#[test]
fn engine_marks_node_departed_when_silent_since_reconnect() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    // A port reopen announces the bridge too. The node says nothing for the whole
    // timeout after it, so it has gone.
    engine.handle(connected(), clock.at(2));
    engine.handle(Event::Tick, clock.at(2 + timeout_s()));
    assert!(departed(&engine, NODE));
}

#[test]
fn engine_keeps_node_undeparted_when_link_down() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    // An outage silences every node at once; that is not the node leaving.
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(2),
    );
    for at in 3..=63 {
        engine.handle(Event::Tick, clock.at(at));
        assert!(!departed(&engine, NODE), "at {at} s");
    }
    // Nor straight after the reconnect, before the node's next heartbeat has said
    // whether it is still there.
    engine.handle(connected(), clock.at(64));
    engine.handle(Event::Tick, clock.at(65));
    assert!(!departed(&engine, NODE));
}

// A host behind the bridge drains its queue as fast as USB will carry it, and every
// heartbeat in it is history (`docs/phase-4-findings.md`): the slot it names may be
// another node's now, and its epoch is one `deal` cannot skip. So the share waits.
#[test]
fn engine_holds_assignment_when_backlog_heartbeats() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);

    // Nine heartbeats a second apart on the bridge's clock, handed to the engine
    // two milliseconds apart on the host's, the first already a second behind.
    let mut sent = 0;
    for beat in 0..9u32 {
        let batch = engine.handle(
            beat_at(
                NODE,
                beat + 1,
                0,
                Capabilities::here(Board::Esp32c5),
                1_000_000 + beat * 1_000_000,
            ),
            clock.at_ms(u64::from(beat) * 2),
        );
        sent += batch.urgent.len();
        assert!(!stored_live(&batch), "beat {beat} is stored as replayed");
    }

    assert_eq!(sent, 0, "every heartbeat read behind, so nothing sent");
    assert!(engine.nodes().next().expect("the node").dirty, "still owed");
    // Admitted regardless: a stale heartbeat still says the node was alive and
    // what it is.
    assert_eq!(engine.nodes().count(), 1);
}

// Once the host reads live, the node's next heartbeat reports an epoch to skip, and
// its share goes out.
#[test]
fn engine_sends_assignment_when_live_heartbeat_follows_backlog() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);

    // A backlog, then the host catches up: it waits a full second of its own
    // clock for a record the bridge stamped a second later.
    for beat in 0..4u32 {
        engine.handle(
            beat_at(
                NODE,
                beat + 1,
                0,
                Capabilities::here(Board::Esp32c5),
                1_000_000 + beat * 1_000_000,
            ),
            clock.at_ms(u64::from(beat) * 2),
        );
    }

    let live = engine.handle(
        beat_at(NODE, 5, 0, Capabilities::here(Board::Esp32c5), 5_000_000),
        clock.at_ms(1_010),
    );
    let (_, dst, _) = sent_admin(&live);
    assert_eq!(dst, NODE);
}

// An epoch a live heartbeat reported does not make a replayed one current: the node
// may have moved slots since, and an assignment carries no MAC to catch that.
#[test]
fn engine_holds_assignment_when_heartbeat_replayed_with_epoch_known() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let caps = Capabilities::here(Board::Esp32c5);
    let first = engine.handle(beat_at(NODE, 1, 0, caps, 1_000), clock.at_ms(1));
    assert!(stored_live(&first));
    let (id, ..) = sent_admin(&first);
    engine.handle(undelivered(id, Undelivered::Timeout), clock.at_ms(1));

    let replayed = engine.handle(beat_at(NODE, 2, 0, caps, 2_000_000), clock.at_ms(2));

    assert!(!stored_live(&replayed), "read behind the bridge");
    assert!(admins(&replayed).is_empty(), "nothing sent on a heartbeat read behind");
    assert!(engine.nodes().next().expect("the node").dirty, "still owed");
}

/// Whether the one heartbeat stored in a batch was stored as live.
fn stored_live(batch: &ActionBatch) -> bool {
    let rows: Vec<bool> = batch
        .records
        .iter()
        .filter_map(|r| match r {
            Record::Heartbeat(h) => Some(h.live),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "exactly one heartbeat row");
    rows[0]
}

// Nothing older than the connection reaches the engine: the bridge clears its queue
// when a host connects, and the transport drops what decodes ahead of `Ready`. So
// the first record after a connect is the present.
#[test]
fn engine_reads_live_when_first_heartbeat_follows_connect() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(0));

    let first = engine.handle(
        beat_at(NODE, 1, 0, Capabilities::here(Board::Esp32c5), 560_000_000),
        clock.at_ms(10),
    );
    let (_, dst, _) = sent_admin(&first);
    assert_eq!(dst, NODE);
    assert!(stored_live(&first), "stored as live");
    let row = host_status(&engine.handle(Event::HostSample(HostSample::default()), clock.at(2)));
    assert_eq!(row.lag_peak_us, 0, "nothing measured a lag");
}

// The two clocks jitter by microseconds. A connection that started behind by
// assumption stayed there on an 11 µs excess, and stored every heartbeat as replayed
// for seconds.
#[test]
fn engine_reads_live_when_bridge_elapsed_slightly_exceeds_host_elapsed_after_connect() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(0));
    engine.handle(stray(0), clock.at(0));

    let beat = engine
        .handle(beat_at(NODE, 1, 0, Capabilities::here(Board::Esp32c5), 1_000_011), clock.at(1));
    let (_, dst, _) = sent_admin(&beat);
    assert_eq!(dst, NODE);
    assert!(stored_live(&beat));
}

// A spell behind the bridge does not outlive its connection: the next one starts
// caught up.
#[test]
fn engine_reads_live_when_link_reconnects_after_falling_behind() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let behind = engine.handle(
        beat_at(NODE, 1, 0, Capabilities::here(Board::Esp32c5), 5_000_000),
        clock.at_ms(10),
    );
    assert!(!stored_live(&behind));

    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(10),
    );
    engine.handle(connected(), clock.at(20));
    let fresh = engine.handle(
        beat_at(NODE, 2, 0, Capabilities::here(Board::Esp32c5), 600_000_000),
        clock.at_ms(20_010),
    );
    let (_, dst, _) = sent_admin(&fresh);
    assert_eq!(dst, NODE);
    assert!(stored_live(&fresh));
}

#[test]
fn engine_reissues_nothing_when_same_bridge_reconnects() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    engine.handle(connected(), clock.at(2));
    engine.handle(stray(0), clock.at(2));

    let batch = engine.handle(heartbeat_holding(NODE, 2, first.epoch), clock.at(3));
    assert!(admins(&batch).is_empty(), "the node already holds this share");
}

// ---------------------------------------------------------------------------
// Command::ClearRing: not an assignment, delivered the same way one is.
// ---------------------------------------------------------------------------

fn clear_ring(mac: Option<Mac>) -> Event {
    Event::Command(Command::ClearRing { mac })
}

#[test]
fn engine_sends_power_cycle_at_once_when_operator_asks() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected_board(NODE, NodeBoard::Carrier14), clock.at(0));
    let slot = Slot::new(1, 0).expect("B1");

    let batch = engine.handle(Event::Command(Command::PowerCycle { slot }), clock.at(1));

    assert_eq!(batch.bulk, [HostToBridge::PowerCycle { slot }]);
    assert!(batch.urgent.is_empty() && batch.records.is_empty());
}

#[test]
fn engine_sends_one_set_fan_and_a_status_poll_when_operator_sets_a_duty() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected_board(NODE, NodeBoard::Carrier14), clock.at(0));
    let duty = FanDuty::new(40).expect("in range");

    let batch = engine.handle(Event::Command(Command::SetFanDuty { duty }), clock.at(1));

    // The bridge sets the duty before it answers the poll, so the `Status` carries it.
    assert_eq!(batch.bulk, [HostToBridge::SetFan { duty }, HostToBridge::GetStatus]);
    assert!(batch.urgent.is_empty() && batch.records.is_empty());
    // The bridge keeps the duty, so nothing re-sends it.
    let later = engine.handle(Event::Tick, clock.at(30));
    assert!(!later.bulk.iter().any(|cmd| matches!(cmd, HostToBridge::SetFan { .. })));
}

#[test]
fn engine_names_power_cycle_outcome_when_bridge_answers() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(connected_board(NODE, NodeBoard::Carrier6), clock.at(0));
    let b1 = Slot::new(1, 0).expect("B1");
    let a5 = Slot::new(0, 4).expect("A5");
    let cases = [
        (b1, PowerOutcome::Done, "N4 power-cycled"),
        (b1, PowerOutcome::NoCarrier, "this bridge has no carrier board, so it cannot switch"),
        (a5, PowerOutcome::NotDriven, "A5 is not a slot on this carrier-6"),
        (b1, PowerOutcome::Failed, "N4 was not power-cycled: the carrier's I/O expander did not"),
    ];
    let mut last_id = 0;
    for (slot, outcome, want) in cases {
        let answer = Event::Link(LinkEvent::Message(BridgeToHost::PowerCycled { slot, outcome }));
        engine.handle(answer, clock.at(1));
        let snapshot = engine.snapshot(clock.at(1), StoreStats::default());
        let said = snapshot.power_answer.expect("an answer");
        assert!(said.text.starts_with(want), "{outcome:?}: {}", said.text);
        // The view shows each answer once, telling them apart by this.
        assert!(said.id > last_id, "{outcome:?}");
        last_id = said.id;
    }
}

#[test]
fn engine_sends_clear_on_next_heartbeat_when_node_clear_requested() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    let command = engine.handle(clear_ring(Some(NODE)), clock.at(2));
    assert!(command.urgent.is_empty(), "nothing sent until the node's own heartbeat");
    assert!(engine.nodes().next().expect("the node").clear_dedup_ring);

    let (_, dst) = sent_clear(&engine.handle(heartbeat_holding(NODE, 2, first.epoch), clock.at(3)));
    assert_eq!(dst, NODE);
    assert_eq!(counters(&engine).admin_sent, 1, "the clear does not count as an assignment");
}

#[test]
fn engine_keeps_clear_owed_when_undelivered() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));
    engine.handle(clear_ring(Some(NODE)), clock.at(2));

    let (id, _) = sent_clear(&engine.handle(heartbeat(NODE, 2), clock.at(3)));
    engine.handle(undelivered(id, Undelivered::Timeout), clock.at(3));
    assert!(engine.nodes().next().expect("the node").clear_dedup_ring, "still owed");

    // Retried on the next heartbeat, the same as an undelivered assignment.
    let (_, dst) = sent_clear(&engine.handle(heartbeat(NODE, 3), clock.at(4)));
    assert_eq!(dst, NODE);
}

#[test]
fn engine_drops_clear_owed_when_delivered() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));
    engine.handle(clear_ring(Some(NODE)), clock.at(2));

    let (id, _) = sent_clear(&engine.handle(heartbeat(NODE, 2), clock.at(3)));
    engine.handle(delivered(id), clock.at(3));
    assert!(!engine.nodes().next().expect("the node").clear_dedup_ring);

    let next = engine.handle(heartbeat(NODE, 3), clock.at(4));
    assert!(clears(&next).is_empty(), "nothing left to send");
}

#[test]
fn engine_sends_one_clear_when_node_beats_twice_unanswered() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, first) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));
    engine.handle(clear_ring(Some(NODE)), clock.at(2));

    let (id, _) = sent_clear(&engine.handle(heartbeat_holding(NODE, 2, first.epoch), clock.at(3)));
    let second = engine.handle(heartbeat_holding(NODE, 3, first.epoch), clock.at(4));
    assert!(clears(&second).is_empty(), "one clear in flight per node");

    engine.handle(undelivered(id, Undelivered::Timeout), clock.at(4));
    let (retry, dst) =
        sent_clear(&engine.handle(heartbeat_holding(NODE, 4, first.epoch), clock.at(5)));
    assert_eq!(dst, NODE);
    assert_ne!(retry, id);
}

#[test]
fn engine_clears_only_assignable_nodes_when_none_named() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 1), clock.at(1));
    // Filed, but a minor behind: not assignable, so not owed a clear.
    engine.handle(beat_at(OTHER, 1, 0, outdated_capabilities(), 0), clock.at(1));

    engine.handle(clear_ring(None), clock.at(2));

    assert!(engine.nodes().find(|n| n.mac == NODE).expect("heartbeating").clear_dedup_ring);
    assert!(
        !engine.nodes().find(|n| n.mac == OTHER).expect("filed").clear_dedup_ring,
        "needs an update, so it is not in the set the planner would cut for either"
    );
}

#[test]
fn engine_drops_clear_owed_when_node_reboots() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 5), clock.at(1));
    engine.handle(clear_ring(Some(NODE)), clock.at(2));
    assert!(engine.nodes().next().expect("the node").clear_dedup_ring);

    // The counter goes backwards: a reboot, and a freshly booted node already
    // holds an empty ring.
    engine.handle(heartbeat(NODE, 1), clock.at(3));
    assert!(!engine.nodes().next().expect("the node").clear_dedup_ring);
}

#[test]
fn engine_holds_clear_when_heartbeat_replayed() {
    // A heartbeat read behind names the slot the node was in then, and a clear carries
    // no MAC, so it waits for a live one.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let caps = Capabilities::here(Board::Esp32c5);
    let first = engine.handle(beat_at(NODE, 1, 0, caps, 1_000), clock.at_ms(1));
    assert!(stored_live(&first));
    engine.handle(clear_ring(Some(NODE)), clock.at_ms(1));

    let batch = engine.handle(beat_at(NODE, 2, 0, caps, 2_000_000), clock.at_ms(2));

    assert!(!stored_live(&batch), "read behind the bridge");
    assert!(clears(&batch).is_empty(), "nothing sent on a heartbeat read behind");
    assert!(engine.nodes().next().expect("the node").clear_dedup_ring, "still owed");
}

#[test]
fn engine_skips_held_epoch_when_dealing_first_assignment() {
    // The first heartbeat reports holding wire epoch 1 -- exactly what `deal` would
    // allocate first under `EngineConfig::default()` (the first counter is 1, and
    // `wire_epoch(1) == 1`). It is answered at once.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    let batch = engine.handle(heartbeat_holding(NODE, 1, 1), clock.at(1));
    let (id, _, admin) = sent_admin(&batch);
    assert_ne!(admin.epoch, 1, "never the epoch the node's own heartbeat already reports");

    engine.handle(delivered(id), clock.at(1));
    engine.handle(heartbeat_holding(NODE, 2, admin.epoch), clock.at(5));
    assert!(engine.nodes().next().expect("the node").adopted());
}

#[test]
fn engine_reissues_when_desired_epoch_matches_held() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(OTHER, 1), clock.at(1));

    // NODE's first heartbeat is a backlog replay (huge bridge-side rx_us jump against
    // a tiny host-clock one): `held_epoch` stays unknown, so `deal` has nothing to skip.
    let batch = engine.handle(
        beat_at(NODE, 1, 0, Capabilities::here(Board::Esp32c5), 1_000_000),
        clock.at_ms(1_002),
    );
    assert!(batch.urgent.is_empty(), "no assignment while the node's epoch is unknown");

    let node = engine.nodes().find(|n| n.mac == NODE).expect("the node");
    let desired = node.desired.expect("dealt while held_epoch was unknown");
    let collide_epoch = wire_epoch(desired.counter);

    // A live heartbeat now reports exactly the epoch just allocated blind. The
    // host waits out the same 4 s the bridge's clock advances (4_000_000 rx_us
    // against 4_000 host ms), which is what reads as caught-up rather than
    // another backlog record; `heartbeat_holding` cannot express this because it
    // always reports rx_us 0.
    let batch = engine.handle(
        beat_at(NODE, 2, collide_epoch, Capabilities::here(Board::Esp32c5), 5_000_000),
        clock.at_ms(5_002),
    );
    let (_, _, admin) = sent_admin(&batch);
    assert_ne!(admin.epoch, collide_epoch, "reissued under a fresh epoch");
}

/// The engine's Bluetooth state as the view sees it: holder, preferred node, remember flag.
fn ble_state(engine: &FleetEngine, now: Now) -> (Option<Mac>, Option<Mac>, bool) {
    let snapshot = engine.snapshot(now, StoreStats::default());
    (snapshot.ble_node, snapshot.preferred_ble, snapshot.remember_ble)
}

#[test]
fn engine_gives_ble_to_preferred_node_when_it_first_heartbeats() {
    // Remembered from the file: no command is sent this run.
    let clock = Clock::new();
    let config = EngineConfig { preferred_ble: Some(peer(0)), ..EngineConfig::default() };
    let mut engine = engine(config, &clock);
    caught_up(&mut engine, &clock);
    assert_eq!(ble_state(&engine, clock.at(0)).0, None, "nothing holds it before it is heard");

    let batch = engine.handle(heartbeat(peer(0), 1), clock.at(1));

    assert_eq!(ble_state(&engine, clock.at(1)).0, Some(peer(0)));
    let (_, dst, admin) = sent_admin(&batch);
    assert_eq!(dst, peer(0));
    assert!(admin.scan_ble(), "its first assignment carries the scan");
}

#[test]
fn engine_gives_ble_back_to_preferred_node_when_it_returns() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(heartbeat(peer(1), 1), clock.at(1));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(2));

    engine.handle(heartbeat(peer(1), 2), clock.at(70));
    engine.handle(Event::Tick, clock.at(70));
    assert_eq!(ble_state(&engine, clock.at(70)).0, None, "the scan ages out with the node");

    let batch = engine.handle(heartbeat(peer(0), 2), clock.at(71));

    assert_eq!(ble_state(&engine, clock.at(71)).0, Some(peer(0)));
    assert!(admins(&batch).iter().any(|admin| admin.scan_ble()), "it is told on its own heartbeat");
}

#[test]
fn engine_remembers_target_when_assign_ble_commanded() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);

    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(1));

    assert_eq!(ble_state(&engine, clock.at(1)), (Some(peer(0)), Some(peer(0)), true));
}

#[test]
fn engine_forgets_preferred_ble_when_scan_taken_off() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(2));

    engine.handle(Event::Command(Command::AssignBle { mac: None }), clock.at(3));
    // Still assignable, so a preferred node left behind would take the scan straight back.
    engine.handle(heartbeat(peer(0), 2), clock.at(4));
    engine.handle(Event::Tick, clock.at(4));

    assert_eq!(ble_state(&engine, clock.at(4)), (None, None, true));
}

#[test]
fn engine_keeps_scan_but_forgets_preferred_when_remember_off() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(2));

    engine.handle(Event::Command(Command::RememberBle { on: false }), clock.at(3));

    assert_eq!(ble_state(&engine, clock.at(3)), (Some(peer(0)), None, false));
}

#[test]
fn engine_ignores_assign_ble_for_preference_when_remember_off() {
    let clock = Clock::new();
    let config = EngineConfig { remember_ble: false, ..EngineConfig::default() };
    let mut engine = engine(config, &clock);

    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(1));

    assert_eq!(ble_state(&engine, clock.at(1)), (Some(peer(0)), None, false));
}

#[test]
fn engine_adopts_current_scanner_when_remember_on() {
    let clock = Clock::new();
    let config = EngineConfig { remember_ble: false, ..EngineConfig::default() };
    let mut engine = engine(config, &clock);
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(1));

    engine.handle(Event::Command(Command::RememberBle { on: true }), clock.at(2));

    assert_eq!(ble_state(&engine, clock.at(2)), (Some(peer(0)), Some(peer(0)), true));

    // Already on: a node remembered from the file and not yet heard from survives.
    let config = EngineConfig { preferred_ble: Some(peer(1)), ..EngineConfig::default() };
    let mut from_file = self::engine(config, &clock);
    from_file.handle(Event::Command(Command::RememberBle { on: true }), clock.at(1));
    assert_eq!(ble_state(&from_file, clock.at(1)), (None, Some(peer(1)), true));
}

#[test]
fn engine_ignores_configured_preferred_ble_when_remember_off() {
    let clock = Clock::new();
    let config = EngineConfig {
        remember_ble: false,
        preferred_ble: Some(peer(0)),
        ..EngineConfig::default()
    };
    let mut engine = engine(config, &clock);

    engine.handle(heartbeat(peer(0), 1), clock.at(1));
    engine.handle(Event::Tick, clock.at(1));

    assert_eq!(ble_state(&engine, clock.at(1)), (None, None, false));
}

// ---------------------------------------------------------------------------
// The pool: the bridge's `Status` sets what the fleet partitions, and `Command::SetPool`
// asks the bridge to change it.
// ---------------------------------------------------------------------------

fn set_pool(pool: ChannelPool) -> Event {
    Event::Command(Command::SetPool { pool })
}

#[test]
fn engine_cuts_no_plan_when_no_status_has_arrived() {
    let clock = Clock::new();
    let mut engine = FleetEngine::new(EngineConfig::default(), clock.at(0));
    caught_up(&mut engine, &clock);
    for counter in 1..=3 {
        let batch = engine.handle(heartbeat(NODE, counter), clock.at(counter.into()));
        assert!(admins(&batch).is_empty(), "no pool, so nothing to deal");
    }
    let snapshot = engine.snapshot(clock.at(3), StoreStats::default());
    assert_eq!((snapshot.pool, snapshot.plan), (None, None));
    assert_eq!(counters(&engine).replans, 0);

    engine.handle(pool_status(ChannelPool::Us), clock.at(4));
    let (_, dst, admin) = sent_admin(&engine.handle(heartbeat(NODE, 4), clock.at(5)));
    assert_eq!(dst, NODE);
    assert_eq!(admin.channels, ChannelPool::Us.channels(), "the pool the bridge reported");
}

#[test]
fn engine_keeps_pool_when_link_drops() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Eu, EngineConfig::default(), &clock);
    engine.handle(connected(), clock.at(1));
    engine.handle(
        Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
        clock.at(2),
    );
    let snapshot = engine.snapshot(clock.at(2), StoreStats::default());
    assert_eq!(snapshot.pool, Some(ChannelPool::Eu));
}

#[test]
fn engine_sends_set_pool_then_status_poll_when_operator_sets_pool() {
    let clock = Clock::new();
    let mut engine = settled_pair(&clock);

    let batch = engine.handle(set_pool(ChannelPool::Us), clock.at(3));

    // The bridge takes the pool before it answers the poll, so the `Status` carries it.
    assert_eq!(
        batch.bulk,
        [HostToBridge::SetPool { pool: ChannelPool::Us }, HostToBridge::GetStatus]
    );
    assert!(batch.urgent.is_empty() && batch.records.is_empty());
    assert_eq!(
        engine.snapshot(clock.at(3), StoreStats::default()).pool,
        Some(ChannelPool::default()),
        "the pool is the bridge's until its `Status` says otherwise"
    );
    assert!(engine.nodes().all(|node| !node.dirty), "nothing re-cut");
}

/// Two nodes on the default pool, each holding its acknowledged share of it.
fn settled_pair(clock: &Clock) -> FleetEngine {
    let mut engine = engine(EngineConfig::default(), clock);
    caught_up(&mut engine, clock);
    // The second node's arrival re-cuts the pool, so the shares that stick are the
    // ones sent on the second heartbeat. The first are delivered and superseded, so
    // none is in flight when the second goes out.
    for n in 0..2 {
        let first = engine.handle(heartbeat(peer(n), 1), clock.at(1));
        deliver_all(&mut engine, &first, clock.at(1));
    }
    for n in 0..2 {
        let (id, ..) = sent_admin(&engine.handle(heartbeat(peer(n), 2), clock.at(2)));
        engine.handle(delivered(id), clock.at(2));
    }
    assert!(engine.nodes().all(|node| !node.dirty), "both settled");
    engine
}

#[test]
fn engine_recuts_fleet_when_status_reports_new_pool() {
    let clock = Clock::new();
    let mut engine = settled_pair(&clock);
    let replans_before = counters(&engine).replans;

    let batch = engine.handle(pool_status(ChannelPool::Us), clock.at(3));
    assert!(batch.urgent.is_empty(), "nothing sent until each node's own heartbeat");

    assert_eq!(covered(&wanted(&engine)), pool_indices(ChannelPool::Us));
    for node in engine.nodes() {
        assert!(node.dirty, "{:02X?} owes its new share", node.mac);
        let desired = node.desired.expect("a share");
        let confirmed = node.confirmed.expect("the old share");
        assert_ne!(desired.counter, confirmed.counter, "under a fresh epoch");
    }
    assert_eq!(counters(&engine).replans, replans_before + 1);
    assert_eq!(engine.snapshot(clock.at(3), StoreStats::default()).pool, Some(ChannelPool::Us));

    let (_, dst, admin) = sent_admin(&engine.handle(heartbeat(peer(0), 3), clock.at(4)));
    assert_eq!(dst, peer(0));
    assert!(ChannelPool::Us.channels().bits() & admin.channels.bits() == admin.channels.bits());
}

#[test]
fn engine_keeps_plan_when_status_reports_same_pool() {
    let clock = Clock::new();
    let mut engine = settled_pair(&clock);
    let replans_before = counters(&engine).replans;
    let counters_before: Vec<u64> =
        engine.nodes().map(|node| node.confirmed.expect("acked").counter).collect();

    let batch = engine.handle(pool_status(ChannelPool::default()), clock.at(3));

    assert!(batch.urgent.is_empty() && batch.bulk.is_empty());
    assert!(engine.nodes().all(|node| !node.dirty), "nothing wanted a fresh epoch");
    let counters_after: Vec<u64> =
        engine.nodes().map(|node| node.confirmed.expect("acked").counter).collect();
    assert_eq!(counters_after, counters_before);
    assert_eq!(counters(&engine).replans, replans_before);
}

#[test]
fn engine_cuts_new_pool_when_first_node_heartbeats_after_status_changes_it() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);

    engine.handle(pool_status(ChannelPool::Us), clock.at(1));
    let snapshot = engine.snapshot(clock.at(1), StoreStats::default());
    assert_eq!(snapshot.pool, Some(ChannelPool::Us));
    assert!(snapshot.plan.is_none(), "nobody to cut for");

    let (_, _, admin) = sent_admin(&engine.handle(heartbeat(peer(0), 1), clock.at(2)));
    assert_eq!(admin.channels, ChannelPool::Us.channels(), "the first cut is of the new pool");
}

#[test]
fn engine_moves_node_into_pool_when_pool_narrowed() {
    // Three nodes on `all` share every channel, so each holds some the US pool leaves out.
    // Keeping them would go on scanning what the bridge just took out of the pool.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    for n in 0..3 {
        engine.handle(heartbeat(peer(n), 1), clock.at(1));
    }
    settle_fleet(&mut engine, &clock, 3, 2);
    let outside = |set: ChannelSet| set.bits() & !ChannelPool::Us.channels().bits() != 0;
    let last = engine.nodes().find(|node| node.mac == peer(2)).expect("here");
    assert!(
        outside(last.confirmed.expect("settled").channels),
        "it holds a channel the new pool leaves out: {last:?}"
    );

    engine.handle(pool_status(ChannelPool::Us), clock.at(20));

    for node in engine.nodes() {
        let held = node.desired.or(node.confirmed).expect("in the plan");
        assert!(!outside(held.channels), "{:02X?} still scans outside the pool", node.mac);
    }
    let last = engine.nodes().find(|node| node.mac == peer(2)).expect("here");
    let held = last.desired.expect("dealt");
    assert!(last.dirty, "and it answers that node's next heartbeat");
    let share = plan(ChannelPool::Us, 3).expect("a valid fleet").channels_for(2);
    assert_eq!(Some(held.channels), share, "its slot's share of the new pool");
}

#[test]
fn engine_keeps_ble_node_unchanged_when_pool_changes() {
    let clock = Clock::new();
    let mut engine = settled_pair(&clock);
    engine.handle(Event::Command(Command::AssignBle { mac: Some(peer(0)) }), clock.at(3));
    let (id, _, scan) = sent_admin(&engine.handle(heartbeat(peer(0), 3), clock.at(4)));
    assert!(scan.scan_ble());
    engine.handle(delivered(id), clock.at(4));
    let (id, ..) = sent_admin(&engine.handle(heartbeat(peer(1), 3), clock.at(4)));
    engine.handle(delivered(id), clock.at(4));

    engine.handle(pool_status(ChannelPool::Us), clock.at(5));

    let scanner = engine.nodes().find(|node| node.mac == peer(0)).expect("here");
    assert!(!scanner.dirty, "an empty share with the flag is the same under any pool");
    let batch = engine.handle(heartbeat_holding(peer(0), 4, scan.epoch), clock.at(6));
    assert!(admins(&batch).is_empty(), "the Bluetooth node is re-sent nothing");

    let (_, dst, admin) = sent_admin(&engine.handle(heartbeat(peer(1), 4), clock.at(7)));
    assert_eq!(dst, peer(1));
    assert_eq!(admin.channels, ChannelPool::Us.channels(), "the only sniffer takes the new pool");
}

thread_local! {
    /// The calling test's captured lines, while [`Logs::during`] runs.
    static CAPTURED: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// The engine's node-membership log lines, each as `field=value` pairs.
///
/// One global subscriber with a per-thread capture, not `with_default` per test: a scoped
/// subscriber coming and going while other tests run lets tracing cache a callsite as disabled.
struct Logs;

impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
            }
        }
        if *event.metadata().level() > tracing::Level::INFO {
            return;
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        // Lag lines are not about membership.
        if fields.0.starts_with("message=node ") {
            CAPTURED.with_borrow_mut(|lines| lines.as_mut().map(|lines| lines.push(fields.0)));
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

impl Logs {
    /// Runs `f`, then returns the lines it logged on this thread.
    fn during(f: impl FnOnce()) -> Vec<String> {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| tracing::subscriber::set_global_default(Logs).expect("no other"));
        // A callsite another thread registered while the sink was being installed can have
        // cached it as disabled; recompute every callsite's interest.
        tracing::callsite::rebuild_interest_cache();
        CAPTURED.set(Some(Vec::new()));
        f();
        CAPTURED.take().unwrap_or_default()
    }
}

#[test]
fn engine_logs_node_joined_when_first_heartbeat_arrives() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);

    let lines = Logs::during(|| {
        engine.handle(heartbeat(NODE, 1), clock.at(1));
    });

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("message=node joined"), "{lines:?}");
    assert!(lines[0].contains(&format!("slot={}", slot_of(NODE))), "{lines:?}");
}

#[test]
fn engine_logs_node_left_when_heartbeats_stop_for_topology_timeout() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    // Eviction waits until the link has been up a whole timeout, which it has by then.
    let t = timeout_s();
    let lines = Logs::during(|| {
        engine.handle(Event::Tick, clock.at(t + 2));
        engine.handle(Event::Tick, clock.at(t + 3));
    });

    assert_eq!(lines.len(), 1, "logged once: {lines:?}");
    assert!(lines[0].contains("message=node left the fleet"), "{lines:?}");
    assert!(lines[0].contains("reason=\"silent\""), "{lines:?}");
    assert!(lines[0].contains(&format!("silent_ms={}", (t + 1) * 1000)), "{lines:?}");
}

#[test]
fn engine_logs_node_returned_when_heartbeat_follows_departure() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));
    engine.handle(Event::Tick, clock.at(2 + timeout_s()));

    let lines = Logs::during(|| {
        engine.handle(heartbeat(NODE, 2), clock.at(421));
        engine.handle(heartbeat(NODE, 3), clock.at(422));
    });

    assert_eq!(lines.len(), 1, "logged once: {lines:?}");
    assert!(lines[0].contains("message=node returned"), "{lines:?}");
    assert!(lines[0].contains("silent_ms=420000"), "{lines:?}");
}

#[test]
fn engine_logs_node_restarted_when_heartbeat_counter_decreases() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 175), clock.at(1));

    let lines = Logs::during(|| {
        engine.handle(heartbeat(NODE, 2), clock.at(2));
        engine.handle(heartbeat(NODE, 3), clock.at(3));
    });

    assert_eq!(lines.len(), 1, "logged once: {lines:?}");
    assert!(lines[0].contains("message=node restarted"), "{lines:?}");
    assert!(lines[0].contains("counter_before=175 counter_after=2"), "{lines:?}");
}

#[test]
fn engine_logs_reset_cause_when_node_restarts() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 175), clock.at(1));

    let msg = HeartbeatMsg { reset_cause: ResetCause::Brownout, ..beat_msg(NODE, 2) };
    let lines = Logs::during(|| {
        engine.handle(beat(msg, 0), clock.at(2));
    });

    assert_eq!(lines.len(), 1, "logged once: {lines:?}");
    assert!(lines[0].contains("message=node restarted"), "{lines:?}");
    assert!(lines[0].contains("reset=brownout"), "{lines:?}");
}

/// A heartbeat from a node that booted for `cause`.
fn booted_beat(src: Mac, counter: u32, cause: ResetCause) -> Event {
    beat(HeartbeatMsg { reset_cause: cause, ..beat_msg(src, counter) }, 0)
}

#[test]
fn engine_records_fault_restart_when_node_restarts_for_brownout() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 175), clock.at(1));
    engine.handle(booted_beat(NODE, 2, ResetCause::Brownout), clock.at(2));

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.fault_restart, Some(ResetCause::Brownout));
    assert_eq!(node.fault_restarts, 1);
}

#[test]
fn engine_ignores_restart_when_operator_reset_node() {
    for cause in [ResetCause::PowerOn, ResetCause::External] {
        let clock = Clock::new();
        let mut engine = engine(EngineConfig::default(), &clock);
        engine.handle(heartbeat(NODE, 175), clock.at(1));
        engine.handle(booted_beat(NODE, 2, cause), clock.at(2));

        let node = engine.nodes().next().expect("the node");
        assert_eq!(node.reboots, 1, "{cause}: still a restart");
        assert_eq!(node.fault_restart, None, "{cause}");
        assert_eq!(node.fault_restarts, 0, "{cause}");
    }
}

#[test]
fn engine_ignores_fault_cause_when_node_joins() {
    // The boot a first heartbeat names may predate the run.
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(booted_beat(NODE, 175, ResetCause::Watchdog), clock.at(1));

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.fault_restart, None);
    assert_eq!(node.fault_restarts, 0);
}

#[test]
fn engine_counts_fault_restarts_when_node_restarts_for_faults_again() {
    let clock = Clock::new();
    let mut engine = engine(EngineConfig::default(), &clock);
    engine.handle(heartbeat(NODE, 175), clock.at(1));
    engine.handle(booted_beat(NODE, 2, ResetCause::Brownout), clock.at(2));
    engine.handle(booted_beat(NODE, 50, ResetCause::Brownout), clock.at(3));
    // A power cycle in between neither clears the fault nor counts as one.
    engine.handle(booted_beat(NODE, 3, ResetCause::PowerOn), clock.at(4));
    engine.handle(booted_beat(NODE, 50, ResetCause::Brownout), clock.at(5));
    engine.handle(booted_beat(NODE, 2, ResetCause::Watchdog), clock.at(6));

    let node = engine.nodes().next().expect("the node");
    assert_eq!(node.reboots, 3);
    assert_eq!(node.fault_restart, Some(ResetCause::Watchdog), "the latest");
    assert_eq!(node.fault_restarts, 2);
}

#[test]
fn engine_logs_nothing_when_heartbeats_arrive_on_time() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));

    let lines = Logs::during(|| {
        for (counter, at) in (2..).zip((6..=181).step_by(5)) {
            engine.handle(heartbeat(NODE, counter), clock.at(at));
            engine.handle(Event::Tick, clock.at(at));
        }
    });

    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn engine_logs_nothing_when_link_outage_holds_heartbeats_past_timeout() {
    let clock = Clock::new();
    let mut engine = engine_on(ChannelPool::Us, EngineConfig::default(), &clock);
    caught_up(&mut engine, &clock);
    let (id, _, _) = sent_admin(&engine.handle(heartbeat(NODE, 1), clock.at(1)));
    engine.handle(delivered(id), clock.at(1));
    engine.handle(heartbeat(NODE, 2), clock.at(31));

    // The node beats on through the outage, and the bridge's backlog delivers those beats late.
    let lines = Logs::during(|| {
        engine.handle(
            Event::Link(LinkEvent::Disconnected { reason: "unplugged".to_owned() }),
            clock.at(40),
        );
        for at in 41..=130 {
            engine.handle(Event::Tick, clock.at(at));
        }
        engine.handle(connected(), clock.at(131));
        engine.handle(heartbeat(NODE, 3), clock.at(131));
        for at in 132..=250 {
            if at % 5 == 0 {
                engine.handle(heartbeat(NODE, at / 5), clock.at(u64::from(at)));
            }
            engine.handle(Event::Tick, clock.at(u64::from(at)));
        }
    });

    assert!(lines.is_empty(), "{lines:?}");
}
