//! The host link's framing.
//!
//! Both sample sets carry one case per variant and panic rather than `ok()` on a full
//! vector: a push that failed silently would drop a variant out of every round trip
//! below and leave the suite passing.

use heapless::{String, Vec};
use wartui_proto::ChannelPool;
use wartui_proto::bus::{
    Assign, Board, Capabilities, DieTemp, DownType, Heartbeat, NodeBoard, RecordKind, Security,
    SightingBatchWriter, SightingMsg, Slot, UPDATE_CHUNK, UPDATE_CHUNK_MAX, UpType, UpdateChunk,
    UpdateState, UpdateStatus, Vdd,
};
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::fleet::{Member, Report, ScanMode, Share};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, FromNodeBody, GPS_FRAME, GPS_WRITE_MAX, GpsBytes,
    GpsWriteBytes, HostToBridge, LINK_PROTO_VERSION, LinkError, LoopPhase, MAX_FRAME,
    MAX_HOST_FRAME, MAX_NODE_DOWN, MAX_NODE_PAYLOAD, PowerOutcome, QueueId, QueueStat, QueueStats,
    RadioInfo, RadioLinkStats, SpikeBody, ToNodeBody, UPDATE_STATUS_MAX, Undelivered, UpdateOp,
    UpdateStatusBytes, cache_chunk_body, crc16, decode_frame, encode_frame, encode_host_frame,
    image_tag,
};
use wartui_proto::radio_link::update::{OP_MAX, Op};
use wartui_proto::reset::ResetCause;
use wartui_proto::spike::{FULL_BODY, SpikeProfile, SpikeStats, body_checks, fill_body};

fn sample_commands() -> Vec<HostToBridge, 16> {
    let assign = Assign { epoch: 3, flags: 0, channels: wartui_proto::plan::ChannelSet::empty() };
    let body = ToNodeBody::from_slice(&assign.encode()).expect("an assignment fits");

    let mut v = Vec::new();
    v.push(HostToBridge::Identify).expect("sample_commands has room");
    v.push(HostToBridge::GetStatus).expect("sample_commands has room");
    v.push(HostToBridge::Reset).expect("sample_commands has room");
    v.push(HostToBridge::GpsWrite {
        bytes: GpsWriteBytes::from_slice(&[0xB5, 0x62, 0x06, 0x8A]).expect("fits"),
    })
    .expect("sample_commands has room");
    v.push(HostToBridge::Update(encoded_op(&Op::Begin { session: 7, len: 1234, sha256: [9; 32] })))
        .expect("sample_commands has room");
    v.push(HostToBridge::ToNode {
        slot: Slot::new(1, 2).expect("B3"),
        id: 0xBEEF,
        kind: DownType::Assign.as_u8(),
        body,
    })
    .expect("sample_commands has room");
    v.push(HostToBridge::PowerCycle { slot: Slot::new(0, 6).expect("A7") })
        .expect("sample_commands has room");
    v.push(HostToBridge::CacheImage(encoded_op(&Op::Begin {
        session: 2,
        len: 905_000,
        sha256: [5; 32],
    })))
    .expect("sample_commands has room");
    v.push(sample_chunk_command()).expect("sample_commands has room");
    v.push(HostToBridge::SetFan { duty: FanDuty::new(40).expect("in range") })
        .expect("sample_commands has room");
    v.push(HostToBridge::SetPool { pool: ChannelPool::Eu }).expect("sample_commands has room");
    v.push(HostToBridge::StartScan { bluetooth: true }).expect("sample_commands has room");
    v.push(HostToBridge::StopScan).expect("sample_commands has room");
    v.push(HostToBridge::SpikeLoad { profile: SpikeProfile::Burst, count: 300 })
        .expect("sample_commands has room");
    v
}

/// A [`HostToBridge::ToNodeChunk`] with every field distinct, so a pin shows their order.
fn sample_chunk_command() -> HostToBridge {
    HostToBridge::ToNodeChunk {
        slot: Slot::new(1, 2).expect("B3"),
        id: 0xBEEF,
        image: 0x0403_0201,
        session: 7,
        offset: 4096,
        len: 1024,
    }
}

/// `op` as a [`HostToBridge::Update`] carries it.
fn encoded_op(op: &Op<'_>) -> UpdateOp {
    let mut buf = [0u8; OP_MAX];
    let len = op.encode(&mut buf);
    UpdateOp::from_slice(&buf[..len]).expect("an op fits")
}

/// A status as a [`BridgeToHost::Update`] carries it.
fn encoded_status(status: UpdateStatus) -> UpdateStatusBytes {
    let mut buf = [0u8; UPDATE_STATUS_MAX];
    let len = Op::Status(status).encode(&mut buf);
    UpdateStatusBytes::from_slice(&buf[..len]).expect("a status fits")
}

/// The postcard bytes between a frame's version byte and its CRC.
fn frame_body(frame: &[u8]) -> std::vec::Vec<u8> {
    let mut decoded = frame.to_vec();
    let len = cobs::decode_in_place(&mut decoded[..frame.len() - 1]).expect("valid cobs");
    decoded[1..len - 2].to_vec()
}

fn sample_events() -> Vec<BridgeToHost, 24> {
    let mut writer = SightingBatchWriter::new(1);
    assert!(writer.push(&SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -50,
        security: Security::Wpa2Psk,
        ssid: b"net",
        ext: &[],
    }));
    let body = FromNodeBody::from_slice(writer.as_bytes()).expect("a batch fits the node ceiling");
    let heartbeat = Heartbeat {
        capabilities: Capabilities::here(Board::Esp32c5),
        mac: [2, 0, 0, 0, 0, 1],
        counter: 42,
        epoch: 0,
        wifi_refused: 0,
        ble_refused: 0,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
        heap_free: 40_000,
    };

    let mut v = Vec::new();
    v.push(BridgeToHost::Ready {
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x60],
        fw_version: String::try_from("0.1.0").expect("short"),
        // Not the defaults: a round trip that only ever carried the zero
        // variant of an enum would pass just as well with the field dropped.
        reset_cause: ResetCause::Watchdog,
        last_phase: LoopPhase::Command,
        uptime_ms: 8_675_309,
        node_board: NodeBoard::NoCarrier,
        radio: None,
        image: None,
    })
    .expect("sample_events has room");
    // A second one, because `Option` and the enums the first case does not reach are the
    // fields a round trip would otherwise pass on without carrying.
    v.push(BridgeToHost::Ready {
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x61],
        fw_version: String::try_from("0.1.0").expect("short"),
        reset_cause: ResetCause::Lockup,
        last_phase: LoopPhase::Pump,
        uptime_ms: 1_000,
        node_board: NodeBoard::Carrier14,
        radio: None,
        image: Some([0xD1; 32]),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::FromNode {
        slot: Slot::new(0, 0).expect("A1"),
        rx_us: 123_456,
        kind: UpType::Sightings.as_u8(),
        body,
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::FromNode {
        slot: Slot::new(1, 6).expect("B7"),
        rx_us: 7,
        kind: UpType::Heartbeat.as_u8(),
        body: FromNodeBody::from_slice(&heartbeat.encode()).expect("fits"),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Delivered { id: 7 }).expect("sample_events has room");
    for reason in
        [Undelivered::NoNode, Undelivered::QueueFull, Undelivered::Timeout, Undelivered::NoImage]
    {
        v.push(BridgeToHost::Undelivered { id: 8, reason }).expect("sample_events has room");
    }
    v.push(BridgeToHost::Status {
        rx_count: 1000,
        dropped_tx: 0,
        uptime_ms: 60_000,
        rx_oversize: 2,
        queues: QueueStats::new(),
        radio_link: RadioLinkStats::default(),
        host_uart_errors: 0,
        fan: Some(FanStats { duty: FanDuty::new(40).expect("in range"), rpm: Some(1830) }),
        pool: ChannelPool::Us,
        scan: ScanMode::WifiBluetooth,
        ble: Slot::new(1, 0),
        present: 0b11_1111,
        members: full_members(),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Log {
        message: String::try_from("bus A slot 3 answered").expect("short"),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Gps { bytes: GpsBytes::from_slice(b"$GNGGA,").expect("fits") })
        .expect("sample_events has room");
    v.push(BridgeToHost::Update(encoded_status(UpdateStatus {
        session: 7,
        state: UpdateState::Receiving,
        next_offset: 4096,
    })))
    .expect("sample_events has room");
    for outcome in
        [PowerOutcome::Done, PowerOutcome::NoCarrier, PowerOutcome::NotDriven, PowerOutcome::Failed]
    {
        let slot = Slot::new(1, 0).expect("B1");
        v.push(BridgeToHost::PowerCycled { slot, outcome }).expect("sample_events has room");
    }
    v.push(BridgeToHost::CacheImage(encoded_status(UpdateStatus {
        session: 2,
        state: UpdateState::Verified,
        next_offset: 905_000,
    })))
    .expect("sample_events has room");
    v.push(BridgeToHost::Fleet(full_report())).expect("sample_events has room");
    v.push(BridgeToHost::SpikeLoad {
        seq: 9,
        body: SpikeBody::from_slice(&[1, 2, 3]).expect("fits"),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::SpikeStats(full_spike_stats())).expect("sample_events has room");
    v
}

/// Spike counters with every field distinct and largest.
fn full_spike_stats() -> SpikeStats {
    SpikeStats {
        seq: u32::MAX,
        uptime_ms: u32::MAX - 9,
        ble_owned_ms: u32::MAX - 10,
        queued_bytes: u32::MAX - 1,
        sent_bytes: u32::MAX - 2,
        credited_bytes: u32::MAX - 3,
        delivered_bytes: u32::MAX - 4,
        bulk_high_water: u32::MAX - 5,
        refused: u32::MAX - 6,
        evicted: u32::MAX - 7,
        mtu: u16::MAX,
        phy: u8::MAX,
        interval_us: u32::MAX - 8,
        radio_link: RadioLinkStats {
            packets: u32::MAX,
            crc_fail: u32::MAX,
            bad_magic: u32::MAX,
            seq_gap: u32::MAX,
            uart_errors: u32::MAX,
            radio_crc_fail: u32::MAX,
            radio_bad_magic: u32::MAX,
            radio_seq_gap: u32::MAX,
            radio_uart_errors: u32::MAX,
        },
    }
}

/// A member in every slot, the most a [`BridgeToHost::Status`] carries.
fn full_members() -> Vec<Member, 14> {
    (0..14)
        .map(|byte| Member {
            slot: Slot::from_u8(byte).expect("a slot"),
            mac: [0x02, 0xFF, 0xFF, 0xFF, 0xFF, byte],
        })
        .collect()
}

/// A report with a share in every slot, the largest a [`BridgeToHost::Fleet`] carries.
fn full_report() -> Report {
    let mut shares = heapless::Vec::new();
    for byte in 0..14 {
        let channels = wartui_proto::plan::ChannelSet::from_bits(u64::MAX);
        let slot = Slot::from_u8(byte).expect("a slot");
        shares.push(Share { slot, epoch: 254, ble: byte == 13, channels }).expect("fits");
    }
    Report {
        mode: ScanMode::WifiBluetooth,
        pool: ChannelPool::All,
        shares,
        unreachable: wartui_proto::plan::ChannelSet::from_bits(u64::MAX),
    }
}

#[test]
fn crc16_matches_ccitt_false_standard_when_given_test_vectors() {
    // The standard check value for CRC-16/CCITT-FALSE.
    assert_eq!(crc16(b"123456789"), 0x29B1);
    assert_eq!(crc16(b""), 0xFFFF);
}

/// The bitwise form `crc16`'s table is built from, as the reference.
fn crc16_bitwise(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

#[test]
fn crc16_matches_bitwise_reference_when_given_any_input() {
    assert_eq!(crc16(&[]), crc16_bitwise(&[]));
    for byte in 0..=u8::MAX {
        assert_eq!(crc16(&[byte]), crc16_bitwise(&[byte]), "{byte:#04x}");
    }
    let all: [u8; 256] = core::array::from_fn(|i| i as u8);
    assert_eq!(crc16(&all), crc16_bitwise(&all));
    // A 1 KB xorshift buffer: a full node payload's worth of mixed bytes.
    let mut state: u32 = 0x1234_5678;
    let noise: [u8; 1024] = core::array::from_fn(|_| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as u8
    });
    assert_eq!(crc16(&noise), crc16_bitwise(&noise));
}

#[test]
fn link_codec_round_trips_all_host_commands_when_encoded_and_decoded() {
    for cmd in sample_commands() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_host_frame(&cmd, &mut out).expect("encodes");
        let decoded: HostToBridge =
            decode_frame(&mut out[..n - 1]).expect("decodes without the terminator");
        assert_eq!(decoded, cmd);
    }
}

#[test]
fn link_codec_round_trips_all_bridge_events_when_encoded_and_decoded() {
    for event in sample_events() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(&event, &mut out).expect("encodes");
        let decoded: BridgeToHost = decode_frame(&mut out[..n - 1]).expect("decodes");
        assert_eq!(decoded, event);
    }
}

#[test]
fn link_codec_terminates_frame_with_single_null_byte_when_cobs_encoded() {
    // This is what makes the terminator unambiguous and resync possible.
    for cmd in sample_commands() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_host_frame(&cmd, &mut out).expect("encodes");
        assert_eq!(out[n - 1], 0, "frame must end with the terminator");
        assert!(!out[..n - 1].contains(&0), "COBS body must not contain a zero");
    }
}

#[test]
fn link_decoder_rejects_frame_when_payload_checksum_is_corrupted() {
    let cmd =
        HostToBridge::GpsWrite { bytes: GpsWriteBytes::from_slice(&[1, 2, 3]).expect("fits") };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");

    // Flip a bit in the body, avoiding the COBS length marker and terminator.
    let mut damaged = out;
    damaged[2] ^= 0x01;
    match decode_frame::<HostToBridge>(&mut damaged[..n - 1]) {
        Err(LinkError::BadChecksum { .. } | LinkError::Malformed | LinkError::Corrupt) => {}
        other => panic!("corruption slipped through as {other:?}"),
    }
}

#[test]
fn link_decoder_rejects_frame_when_protocol_version_mismatches() {
    let cmd = HostToBridge::GetStatus;
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");

    // Rebuild the frame by hand with a bumped version, so the CRC stays valid
    // and the version check is genuinely what rejects it.
    let mut body = [0u8; 64];
    let mut decoded = out;
    let len = cobs::decode_in_place(&mut decoded[..n - 1]).expect("valid cobs");
    body[..len].copy_from_slice(&decoded[..len]);
    body[0] = LINK_PROTO_VERSION.wrapping_add(1);
    let crc = crc16(&body[..len - 2]);
    body[len - 2..len].copy_from_slice(&crc.to_le_bytes());

    let mut reframed = [0u8; MAX_FRAME];
    let encoded = cobs::try_encode(&body[..len], &mut reframed).expect("fits");
    assert_eq!(
        decode_frame::<HostToBridge>(&mut reframed[..encoded]),
        Err(LinkError::VersionMismatch {
            ours: LINK_PROTO_VERSION,
            theirs: LINK_PROTO_VERSION.wrapping_add(1)
        })
    );
}

#[test]
fn frame_accumulator_delivers_sequential_frames_when_processing_continuous_stream() {
    let mut stream = std::vec::Vec::new();
    let cmds = sample_commands();
    for cmd in &cmds {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_host_frame(cmd, &mut out).expect("encodes");
        stream.extend_from_slice(&out[..n]);
    }

    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut got = std::vec::Vec::new();
    for byte in stream {
        if let Some(frame) = acc.push(byte) {
            got.push(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(got, cmds.into_iter().collect::<std::vec::Vec<_>>());
}

#[test]
fn frame_accumulator_resynchronizes_cleanly_when_stream_contains_bootloader_noise() {
    // A bridge reset sprays ROM bootloader chatter down the same pipe before
    // the first real frame. Nothing before the next terminator should survive,
    // and everything after it should.
    let cmd = HostToBridge::GpsWrite {
        bytes: GpsWriteBytes::from_slice(&[0xa, 0xb, 0xc]).expect("fits"),
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");

    let mut stream = b"ESP-ROM:esp32c6-20220919\nwaiting for download\r\n".to_vec();
    stream.extend_from_slice(&out[..n]);

    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut got = std::vec::Vec::new();
    for byte in stream {
        if let Some(frame) = acc.push(byte) {
            // The banner has no terminator, so it is still buffered ahead of
            // our frame and must fail rather than decode as something.
            if let Ok(msg) = decode_frame::<HostToBridge>(frame) {
                got.push(msg);
            }
        }
    }
    assert!(got.is_empty(), "the banner ran into the frame, so nothing should decode");

    // The very next frame, after that terminator, decodes cleanly.
    let mut got = std::vec::Vec::new();
    for byte in out[..n].iter().copied() {
        if let Some(frame) = acc.push(byte) {
            got.push(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(got, std::vec![cmd]);
}

#[test]
fn frame_accumulator_ignores_leading_zeros_when_accumulating_stream() {
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    for _ in 0..16 {
        assert!(acc.push(0).is_none(), "empty frames must not be emitted");
    }
    let cmd = HostToBridge::Reset;
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    let mut got = None;
    for byte in out[..n].iter().copied() {
        if let Some(frame) = acc.push(byte) {
            got = Some(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(got, Some(cmd));
}

#[test]
fn frame_accumulator_discards_oversized_frame_when_capacity_is_exceeded() {
    let mut acc = FrameAccumulator::<64>::new();
    for _ in 0..200 {
        assert!(acc.push(0xAB).is_none());
    }
    assert!(acc.push(0).is_none(), "an overrun frame must be dropped, not truncated");

    // And the accumulator is usable immediately afterwards.
    let cmd = HostToBridge::GetStatus;
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    let mut got = None;
    for byte in out[..n].iter().copied() {
        if let Some(frame) = acc.push(byte) {
            got = Some(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(got, Some(cmd));
}

#[test]
fn frame_accumulator_decodes_following_frame_when_truncated_frame_is_terminated() {
    // A frame cut short and closed with its own `0x00` fails alone. Without that terminator
    // the fragment would run into the next frame and cost both.
    let first = HostToBridge::GpsWrite { bytes: GpsWriteBytes::from_slice(&[1; 6]).expect("fits") };
    let second = HostToBridge::GetStatus;
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&first, &mut out).expect("encodes");
    let mut stream = out[..n / 2].to_vec();
    stream.push(0x00);
    let n = encode_host_frame(&second, &mut out).expect("encodes");
    stream.extend_from_slice(&out[..n]);

    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut got = std::vec::Vec::new();
    for byte in stream {
        if let Some(frame) = acc.push(byte)
            && let Ok(msg) = decode_frame::<HostToBridge>(frame)
        {
            got.push(msg);
        }
    }
    assert_eq!(got, std::vec![second]);
}

#[test]
fn frame_accumulator_yields_frame_when_terminator_arrives() {
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&HostToBridge::GetStatus, &mut out).expect("encodes");
    for &byte in &out[..n - 1] {
        assert!(acc.push(byte).is_none());
    }
    assert!(acc.push(0).is_some());
}

#[test]
fn frame_accumulator_yields_nothing_when_overflowing_frame_terminates() {
    // An overlong frame is still arriving until its terminator, whatever the
    // accumulator kept of it.
    let mut acc = FrameAccumulator::<64>::new();
    for _ in 0..200 {
        acc.push(0xAB);
    }
    assert!(acc.push(0).is_none());
}

/// What the host recovers from a reset's banner, an optional lone `0x00`, then one
/// frame. The banner is 1.5 KB with no `0x00`, as measured off a rebooting C6.
fn after_banner(delimited: bool) -> std::vec::Vec<BridgeToHost> {
    let ready = sample_events().into_iter().next().expect("Ready is first");
    assert!(matches!(ready, BridgeToHost::Ready { .. }));
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&ready, &mut out).expect("encodes");

    let mut stream = std::vec![b'E'; 1500];
    if delimited {
        stream.push(0x00);
    }
    stream.extend_from_slice(&out[..n]);

    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut got = std::vec::Vec::new();
    for byte in stream {
        if let Some(frame) = acc.push(byte)
            && let Ok(msg) = decode_frame::<BridgeToHost>(frame)
        {
            got.push(msg);
        }
    }
    got
}

#[test]
fn frame_accumulator_loses_first_frame_when_overlong_banner_is_undelimited() {
    // The banner overflows the buffer, and the overflow ends at the frame's own
    // terminator, so the frame goes with it.
    assert!(after_banner(false).is_empty());
}

#[test]
fn frame_accumulator_decodes_first_frame_when_overlong_banner_is_delimited() {
    // A lone `0x00` is an empty frame, skipped, and it ends the overflowed run.
    assert!(matches!(after_banner(true)[..], [BridgeToHost::Ready { .. }]));
}

#[test]
fn link_decoder_rejects_payload_when_frame_is_truncated() {
    let mut empty: [u8; 0] = [];
    assert!(decode_frame::<HostToBridge>(&mut empty).is_err());
    let mut tiny = [1u8, 2];
    assert!(decode_frame::<HostToBridge>(&mut tiny).is_err());
}

#[test]
fn link_round_trips_to_node_when_body_is_full() {
    let body = ToNodeBody::from_slice(&[0xA5; MAX_NODE_DOWN]).expect("fits");
    let cmd = HostToBridge::ToNode {
        slot: Slot::new(0, 5).expect("A6"),
        id: 7,
        kind: DownType::UpdateChunk.as_u8(),
        body,
    };
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    // Through the accumulator the bridge uses, so a frame that outgrew it would be
    // dropped here rather than on the bench.
    let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
    let mut decoded = None;
    for &byte in &out[..n] {
        if let Some(frame) = acc.push(byte) {
            decoded = Some(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(decoded, Some(cmd));
}

#[test]
fn link_round_trips_from_node_when_body_is_full_and_has_no_zeros() {
    // COBS grows a frame by one byte per 254 without a zero, so a full node body with
    // none is the longest bridge-to-host frame there is.
    let body = FromNodeBody::from_slice(&[0xFF; MAX_NODE_PAYLOAD]).expect("fits");
    let event = BridgeToHost::FromNode {
        slot: Slot::new(1, 6).expect("B7"),
        rx_us: u32::MAX,
        kind: 0xFF,
        body,
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("encodes");
    // Through the accumulator the host uses, so a frame that outgrew it would be
    // dropped here rather than on the bench.
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut decoded = None;
    for &byte in &out[..n] {
        if let Some(frame) = acc.push(byte) {
            decoded = Some(decode_frame::<BridgeToHost>(frame).expect("decodes"));
        }
    }
    assert_eq!(decoded, Some(event));
}

#[test]
fn link_refuses_to_node_body_when_longer_than_max_node_down() {
    assert!(ToNodeBody::from_slice(&[0; MAX_NODE_DOWN + 1]).is_err());
}

#[test]
fn link_refuses_slot_when_byte_names_none() {
    // Postcard: `FromNode`'s index, then the slot byte. A slot past the last fails the decode.
    let event = BridgeToHost::FromNode {
        slot: Slot::new(1, 6).expect("B7"),
        rx_us: 0,
        kind: 1,
        body: FromNodeBody::new(),
    };
    let mut body = [0u8; 64];
    let used = postcard::to_slice(&event, &mut body).expect("fits");
    assert_eq!(&used[..2], &[6, 13]);
    let mut bad = used.to_vec();
    bad[1] = 14;
    assert!(postcard::from_bytes::<BridgeToHost>(&bad).is_err());
}

#[test]
fn link_codec_pins_variant_indices_when_encoded() {
    // Postcard writes a variant as its index, so these are the wire.
    let mut buf = [0u8; MAX_HOST_FRAME];
    let commands = sample_commands();
    assert_eq!(commands.len(), 14, "one of each");
    for (want, cmd) in (0u8..).zip(&commands) {
        assert_eq!(postcard::to_slice(cmd, &mut buf).expect("encodes")[0], want, "{cmd:?}");
    }
    let events = [
        (
            6,
            BridgeToHost::FromNode {
                slot: Slot::new(0, 0).expect("A1"),
                rx_us: 0,
                kind: 1,
                body: FromNodeBody::new(),
            },
        ),
        (7, BridgeToHost::Delivered { id: 1 }),
        (8, BridgeToHost::Undelivered { id: 1, reason: Undelivered::NoNode }),
        (
            9,
            BridgeToHost::PowerCycled {
                slot: Slot::new(0, 0).expect("A1"),
                outcome: PowerOutcome::Done,
            },
        ),
        (10, BridgeToHost::CacheImage(UpdateStatusBytes::new())),
        (11, BridgeToHost::Fleet(full_report())),
        (12, BridgeToHost::SpikeLoad { seq: 0, body: SpikeBody::new() }),
        (13, BridgeToHost::SpikeStats(SpikeStats::default())),
    ];
    for (want, event) in &events {
        assert_eq!(postcard::to_slice(event, &mut buf).expect("encodes")[0], *want, "{event:?}");
    }
    let undelivered = BridgeToHost::Undelivered { id: 1, reason: Undelivered::Timeout };
    assert_eq!(postcard::to_slice(&undelivered, &mut buf).expect("encodes"), [8, 1, 2]);
    let no_image = BridgeToHost::Undelivered { id: 1, reason: Undelivered::NoImage };
    assert_eq!(postcard::to_slice(&no_image, &mut buf).expect("encodes"), [8, 1, 3]);
}

#[test]
fn link_round_trips_gps_when_frame_is_full() {
    let event =
        BridgeToHost::Gps { bytes: GpsBytes::from_slice(&[0xFF; GPS_FRAME]).expect("fits") };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("encodes");
    let decoded: BridgeToHost = decode_frame(&mut out[..n - 1]).expect("decodes");
    assert_eq!(decoded, event);
}

#[test]
fn link_round_trips_gps_write_when_bytes_are_full() {
    let cmd = HostToBridge::GpsWrite {
        bytes: GpsWriteBytes::from_slice(&[0xFF; GPS_WRITE_MAX]).expect("fits"),
    };
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    let decoded: HostToBridge = decode_frame(&mut out[..n - 1]).expect("decodes");
    assert_eq!(decoded, cmd);
}

#[test]
fn link_round_trips_status_when_rx_oversize_is_set() {
    let status = BridgeToHost::Status {
        rx_count: 10,
        dropped_tx: 0,
        uptime_ms: 5_000,
        rx_oversize: 0xDEAD_BEEF,
        queues: QueueStats::new(),
        radio_link: RadioLinkStats::default(),
        host_uart_errors: 0,
        fan: None,
        pool: ChannelPool::Eu,
        scan: ScanMode::Stopped,
        ble: None,
        present: 0,
        members: Vec::new(),
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&status, &mut out).expect("encodes");
    let decoded: BridgeToHost = decode_frame(&mut out[..n - 1]).expect("decodes");
    assert_eq!(decoded, status);
}

fn round_trip(event: &BridgeToHost) -> BridgeToHost {
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(event, &mut out).expect("encodes");
    decode_frame(&mut out[..n - 1]).expect("decodes")
}

#[test]
fn queue_id_lists_every_variant_when_all_read() {
    // The match is exhaustive, so a new variant fails to compile here until `ALL` lists it.
    for (index, id) in QueueId::ALL.into_iter().enumerate() {
        let declared = match id {
            QueueId::RadioUp => 0,
            QueueId::RadioDown => 1,
            QueueId::HostPriority => 2,
            QueueId::HostBulk => 3,
            QueueId::NodeDown => 4,
        };
        assert_eq!(index, declared, "{id:?}");
    }
}

#[test]
fn queue_id_places_queue_on_p4_when_it_runs_there() {
    let on_p4: std::vec::Vec<_> = QueueId::ALL.into_iter().filter(|id| id.on_p4()).collect();
    assert_eq!(
        on_p4,
        [QueueId::RadioDown, QueueId::HostPriority, QueueId::HostBulk, QueueId::NodeDown]
    );
}

#[test]
fn queue_id_round_trips_name_when_every_id_named() {
    for id in QueueId::ALL {
        assert_eq!(QueueId::from_name(id.name()), Some(id), "{id:?}");
    }
    assert_eq!(QueueId::from_name("host"), None);
}

#[test]
fn link_round_trips_status_with_queues_and_radio_link() {
    let mut queues = QueueStats::new();
    for (i, queue) in (1u32..).zip(QueueId::ALL.into_iter().cycle().take(8)) {
        queues
            .push(QueueStat {
                queue,
                capacity: 262_144 * i,
                high_water: 0xFFFF_0000 + i,
                dropped: u32::MAX - i,
            })
            .expect("eight fit");
    }
    let status = BridgeToHost::Status {
        rx_count: 10,
        dropped_tx: 0,
        uptime_ms: 5_000,
        rx_oversize: 0,
        queues,
        radio_link: RadioLinkStats {
            packets: u32::MAX,
            crc_fail: 1,
            bad_magic: 2,
            seq_gap: 3,
            uart_errors: 4,
            radio_crc_fail: 5,
            radio_bad_magic: 6,
            radio_seq_gap: 7,
            radio_uart_errors: 8,
        },
        host_uart_errors: 9,
        fan: Some(FanStats { duty: FanDuty::FULL, rpm: None }),
        pool: ChannelPool::All,
        scan: ScanMode::Stopped,
        ble: None,
        present: u32::MAX,
        members: Vec::new(),
    };
    assert_eq!(round_trip(&status), status);
}

#[test]
fn link_fits_status_in_one_frame_when_every_field_is_largest() {
    // Every varint at its longest, every queue reported and a member in every slot.
    let mut queues = QueueStats::new();
    for queue in QueueId::ALL.into_iter().cycle().take(8) {
        let stat = QueueStat { queue, capacity: u32::MAX, high_water: u32::MAX, dropped: u32::MAX };
        queues.push(stat).expect("eight fit");
    }
    let status = BridgeToHost::Status {
        rx_count: u32::MAX,
        dropped_tx: u32::MAX,
        uptime_ms: u32::MAX,
        rx_oversize: u32::MAX,
        queues,
        radio_link: RadioLinkStats {
            packets: u32::MAX,
            crc_fail: u32::MAX,
            bad_magic: u32::MAX,
            seq_gap: u32::MAX,
            uart_errors: u32::MAX,
            radio_crc_fail: u32::MAX,
            radio_bad_magic: u32::MAX,
            radio_seq_gap: u32::MAX,
            radio_uart_errors: u32::MAX,
        },
        host_uart_errors: u32::MAX,
        fan: Some(FanStats { duty: FanDuty::FULL, rpm: Some(u16::MAX) }),
        pool: ChannelPool::All,
        scan: ScanMode::WifiBluetooth,
        ble: Slot::from_u8(13),
        present: u32::MAX,
        members: full_members(),
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&status, &mut out).expect("fits one frame");
    assert!(n <= MAX_FRAME);
    assert_eq!(round_trip(&status), status);
}

#[test]
fn link_round_trips_ready_with_radio() {
    let ready = BridgeToHost::Ready {
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x62],
        fw_version: String::try_from("0.1.0").expect("short"),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::RadioLink,
        uptime_ms: 2,
        node_board: NodeBoard::Carrier10,
        radio: Some(RadioInfo {
            mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x63],
            fw_version: String::try_from("0.1.1").expect("short"),
            reset_cause: ResetCause::Watchdog,
            last_phase: LoopPhase::Command,
            uptime_ms: u32::MAX,
        }),
        image: Some([0xA5; 32]),
    };
    assert_eq!(round_trip(&ready), ready);
}

#[test]
fn link_codec_pins_update_bytes_when_host_sends_an_op() {
    // Postcard's variant index, then the op as a byte string: its length, then the
    // `radio_link::update` layout.
    let cmd = HostToBridge::Update(encoded_op(&Op::Commit { session: 0x42 }));
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [4, 2, 3, 0x42]);
}

#[test]
fn link_codec_pins_update_bytes_when_bridge_sends_a_status() {
    let status =
        UpdateStatus { session: 0x42, state: UpdateState::Verified, next_offset: 0x0001_2000 };
    let event = BridgeToHost::Update(encoded_status(status));
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [5, 7, 5, 0x42, 2, 0x00, 0x20, 0x01, 0x00]);
}

#[test]
fn link_codec_pins_power_cycle_bytes_when_host_asks() {
    // Postcard's variant index, then the slot's byte: B3 is 9.
    let cmd = HostToBridge::PowerCycle { slot: Slot::new(1, 2).expect("B3") };
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [6, 9]);
}

#[test]
fn link_codec_pins_set_fan_bytes_when_host_sets_a_duty() {
    // Postcard's variant index, then the duty's byte.
    let cmd = HostToBridge::SetFan { duty: FanDuty::new(40).expect("in range") };
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [9, 40]);
}

#[test]
fn link_refuses_set_fan_when_duty_is_over_100() {
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&HostToBridge::SetFan { duty: FanDuty::FULL }, &mut out)
        .expect("encodes");
    let mut body = frame_body(&out[..n]);
    assert_eq!(body, [9, 100]);
    body[1] = 101;
    assert!(postcard::from_bytes::<HostToBridge>(&body).is_err());
}

#[test]
fn link_codec_pins_fan_pool_scan_ble_present_and_members_bytes_at_end_of_status_when_bridge_reports_them()
 {
    // `fan`: `None` is one tag byte; `Some` is the tag, the duty, then `rpm`'s tag and varint.
    // `pool` and `scan`: postcard's variant indices. `ble`: an option's tag, then the slot's byte.
    // `present`: a varint. `members`: a count, then each member's slot byte and MAC, last.
    let mut out = [0u8; MAX_FRAME];
    let mut status = BridgeToHost::Status {
        rx_count: 0,
        dropped_tx: 0,
        uptime_ms: 0,
        rx_oversize: 0,
        queues: QueueStats::new(),
        radio_link: RadioLinkStats::default(),
        host_uart_errors: 0,
        fan: None,
        pool: ChannelPool::All,
        scan: ScanMode::Stopped,
        ble: None,
        present: 0,
        members: Vec::new(),
    };
    let n = encode_frame(&status, &mut out).expect("encodes");
    let none = frame_body(&out[..n]);
    assert_eq!(none[none.len() - 6..], [0, 2, 0, 0, 0, 0]);
    if let BridgeToHost::Status { fan, pool, scan, ble, present, members, .. } = &mut status {
        *fan = Some(FanStats { duty: FanDuty::new(40).expect("in range"), rpm: Some(300) });
        *pool = ChannelPool::Us;
        *scan = ScanMode::WifiBluetooth;
        *ble = Slot::new(1, 2);
        // A1, A3 and B3: bits 0, 2 and 9, 0x205.
        *present = 1 << 0 | 1 << 2 | 1 << 9;
        // A3, then B3: each its slot's byte, then the MAC's six bytes.
        *members = [(2, 0x03), (9, 0x0B)]
            .into_iter()
            .map(|(byte, tail)| Member {
                slot: Slot::from_u8(byte).expect("a slot"),
                mac: [0x02, 0, 0, 0, 0, tail],
            })
            .collect();
    }
    let n = encode_frame(&status, &mut out).expect("encodes");
    let some = frame_body(&out[..n]);
    assert_eq!(some[..none.len() - 6], none[..none.len() - 6]);
    assert_eq!(
        some[none.len() - 6..],
        [
            1, 40, 1, 0xAC, 0x02, 0, 2, 1, 9, 0x85, 0x04, 2, 2, 0x02, 0, 0, 0, 0, 0x03, 9, 0x02, 0,
            0, 0, 0, 0x0B
        ]
    );
    assert_eq!(round_trip(&status), status);
}

#[test]
fn link_codec_pins_set_pool_bytes_when_host_sets_a_pool() {
    // Postcard's variant index, then the pool's.
    let mut out = [0u8; MAX_HOST_FRAME];
    for (pool, byte) in [(ChannelPool::Us, 0), (ChannelPool::Eu, 1), (ChannelPool::All, 2)] {
        let n = encode_host_frame(&HostToBridge::SetPool { pool }, &mut out).expect("encodes");
        assert_eq!(frame_body(&out[..n]), [10, byte], "{pool}");
    }
}

#[test]
fn link_codec_pins_scan_bytes_when_host_starts_or_stops() {
    // Postcard's variant index, then `bluetooth`'s byte.
    let mut out = [0u8; MAX_HOST_FRAME];
    for (bluetooth, byte) in [(false, 0), (true, 1)] {
        let cmd = HostToBridge::StartScan { bluetooth };
        let n = encode_host_frame(&cmd, &mut out).expect("encodes");
        assert_eq!(frame_body(&out[..n]), [11, byte]);
    }
    let n = encode_host_frame(&HostToBridge::StopScan, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [12]);
}

#[test]
fn link_codec_pins_fleet_bytes_when_bridge_reports_a_share() {
    // Postcard's variant index; `mode`'s and `pool`'s; the share count; each share's slot byte,
    // epoch, flag and channel bits as a varint; then `unreachable`'s bits.
    let mut shares = heapless::Vec::new();
    let channels = wartui_proto::plan::ChannelSet::from_bits(0b101);
    shares
        .push(Share { slot: Slot::new(1, 2).expect("B3"), epoch: 7, ble: false, channels })
        .unwrap();
    let report = Report {
        mode: ScanMode::Wifi,
        pool: ChannelPool::Eu,
        shares,
        unreachable: wartui_proto::plan::ChannelSet::empty(),
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&BridgeToHost::Fleet(report.clone()), &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [11, 1, 1, 1, 9, 7, 0, 5, 0]);
    assert_eq!(round_trip(&BridgeToHost::Fleet(report.clone())), BridgeToHost::Fleet(report));
}

#[test]
fn link_round_trips_fleet_within_max_frame_when_every_slot_holds_a_share() {
    let event = BridgeToHost::Fleet(full_report());
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("a full report fits MAX_FRAME");
    assert!(n <= MAX_FRAME);
    assert_eq!(round_trip(&event), event);
}

#[test]
fn link_codec_pins_power_cycled_bytes_when_bridge_answers() {
    let mut out = [0u8; MAX_FRAME];
    for (outcome, code) in [
        (PowerOutcome::Done, 0),
        (PowerOutcome::NoCarrier, 1),
        (PowerOutcome::NotDriven, 2),
        (PowerOutcome::Failed, 3),
    ] {
        let event = BridgeToHost::PowerCycled { slot: Slot::new(0, 6).expect("A7"), outcome };
        let n = encode_frame(&event, &mut out).expect("encodes");
        assert_eq!(frame_body(&out[..n]), [9, 6, code], "{outcome:?}");
    }
}

#[test]
fn link_codec_pins_node_board_ahead_of_radio_and_image_when_ready_is_encoded() {
    // The sample has no radio and no image, so `node_board`'s byte is followed only by their
    // two `None` tags.
    let mut out = [0u8; MAX_FRAME];
    for (board, code) in [
        (NodeBoard::NoCarrier, 0),
        (NodeBoard::Carrier6, 1),
        (NodeBoard::Carrier10, 2),
        (NodeBoard::Carrier14, 3),
    ] {
        let mut ready = sample_events().into_iter().next().expect("Ready is first");
        if let BridgeToHost::Ready { node_board, .. } = &mut ready {
            *node_board = board;
        }
        let n = encode_frame(&ready, &mut out).expect("encodes");
        assert_eq!(frame_body(&out[..n]).last_chunk(), Some(&[code, 0, 0]), "{board:?}");
    }
}

#[test]
fn link_refuses_ready_when_node_board_byte_names_none() {
    let ready = sample_events().into_iter().next().expect("Ready is first");
    let mut buf = [0u8; MAX_FRAME];
    let mut bytes = postcard::to_slice(&ready, &mut buf).expect("encodes").to_vec();
    // `node_board`, ahead of the radio's and the image's `None` tags.
    let at = bytes.len() - 3;
    bytes[at] = 4;
    assert!(postcard::from_bytes::<BridgeToHost>(&bytes).is_err());
}

#[test]
fn link_round_trips_update_when_op_is_a_full_chunk() {
    let data = [0xFF; UPDATE_CHUNK];
    let cmd =
        HostToBridge::Update(encoded_op(&Op::Chunk { session: 3, offset: 8192, data: &data }));
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    // Through the accumulator the P4 uses, so an op that outgrew it would be dropped here
    // rather than on the bench.
    let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
    let mut decoded = None;
    for &byte in &out[..n] {
        if let Some(frame) = acc.push(byte) {
            decoded = Some(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(decoded, Some(cmd));
}

#[test]
fn link_round_trips_ready_when_image_is_present_or_absent() {
    for image in [None, Some([0x5A; 32])] {
        let ready = BridgeToHost::Ready {
            mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x62],
            fw_version: String::try_from("0.1.0").expect("short"),
            reset_cause: ResetCause::Software,
            last_phase: LoopPhase::Boot,
            uptime_ms: 3,
            node_board: NodeBoard::Carrier6,
            radio: None,
            image,
        };
        assert_eq!(round_trip(&ready), ready);
    }
}

#[test]
fn link_tonodechunk_round_trips_through_postcard() {
    let cmd = sample_chunk_command();
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    let decoded: HostToBridge = decode_frame(&mut out[..n - 1]).expect("decodes");
    assert_eq!(decoded, cmd);
}

#[test]
fn link_codec_pins_tonodechunk_bytes_when_host_asks() {
    // Postcard's variant index, the slot's byte (B3 is 9), then varints: id 0xBEEF, the tag
    // 0x04030201, the session, offset 4096 and length 1024.
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&sample_chunk_command(), &mut out).expect("encodes");
    assert_eq!(
        frame_body(&out[..n]),
        [8, 9, 0xEF, 0xFD, 0x02, 0x81, 0x84, 0x8C, 0x20, 7, 0x80, 0x20, 0x80, 0x08]
    );
}

#[test]
fn link_cacheimage_round_trips() {
    let data = [0xA5; UPDATE_CHUNK];
    let cmd =
        HostToBridge::CacheImage(encoded_op(&Op::Chunk { session: 3, offset: 8192, data: &data }));
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    // Through the accumulator the P4 uses, so a full chunk that outgrew it fails here.
    let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
    let mut decoded = None;
    for &byte in &out[..n] {
        if let Some(frame) = acc.push(byte) {
            decoded = Some(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(decoded, Some(cmd));

    let status = UpdateStatus { session: 3, state: UpdateState::Receiving, next_offset: 9216 };
    let event = BridgeToHost::CacheImage(encoded_status(status));
    assert_eq!(round_trip(&event), event);
}

#[test]
fn link_codec_pins_cacheimage_bytes_when_host_sends_an_op() {
    let cmd = HostToBridge::CacheImage(encoded_op(&Op::Commit { session: 0x42 }));
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [7, 2, 3, 0x42]);
}

#[test]
fn link_codec_pins_cacheimage_bytes_when_bridge_sends_a_status() {
    let status =
        UpdateStatus { session: 0x42, state: UpdateState::Verified, next_offset: 0x0001_2000 };
    let event = BridgeToHost::CacheImage(encoded_status(status));
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [10, 7, 5, 0x42, 2, 0x00, 0x20, 0x01, 0x00]);
}

#[test]
fn image_tag_reads_first_four_digest_bytes_little_endian() {
    let mut sha256 = [0xFF; 32];
    sha256[..4].copy_from_slice(&[1, 2, 3, 4]);
    assert_eq!(image_tag(&sha256), 0x0403_0201);
}

/// An image whose bytes all differ from their neighbors, so an offset off by one shows.
fn counting_image(len: usize) -> std::vec::Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn cache_chunk_body_matches_update_chunk_encoding() {
    let image = counting_image(3000);
    // A full chunk, and the short one that ends the image.
    for (offset, len) in [(1024u32, 1024u16), (2048, 952)] {
        let mut ours = [0u8; UPDATE_CHUNK_MAX];
        let n = cache_chunk_body(&image, 9, offset, len, &mut ours).expect("in range");
        let start = offset as usize;
        let chunk = UpdateChunk { session: 9, offset, data: &image[start..start + len as usize] };
        let mut want = [0u8; UPDATE_CHUNK_MAX];
        let m = chunk.encode_into(&mut want).expect("encodes");
        assert_eq!(&ours[..n], &want[..m], "offset {offset}");
    }
}

#[test]
fn cache_chunk_body_refuses_range_past_image_end() {
    let image = counting_image(3000);
    let mut out = [0u8; UPDATE_CHUNK_MAX];
    assert_eq!(cache_chunk_body(&image, 1, 2048, 953, &mut out), None);
    assert_eq!(cache_chunk_body(&image, 1, 3000, 1, &mut out), None);
    assert_eq!(cache_chunk_body(&image, 1, u32::MAX, 1024, &mut out), None);
    assert_eq!(out, [0; UPDATE_CHUNK_MAX], "nothing written");
}

#[test]
fn cache_chunk_body_refuses_zero_and_oversize_len() {
    let image = counting_image(4096);
    let mut out = [0u8; UPDATE_CHUNK_MAX];
    assert_eq!(cache_chunk_body(&image, 1, 0, 0, &mut out), None);
    let oversize = u16::try_from(UPDATE_CHUNK + 1).expect("fits");
    assert_eq!(cache_chunk_body(&image, 1, 0, oversize, &mut out), None);
    assert_eq!(out, [0; UPDATE_CHUNK_MAX], "nothing written");
}

#[test]
fn link_codec_pins_spike_load_bytes_when_host_asks_for_a_burst() {
    // Postcard's variant index, the profile's, then the count as a varint.
    let cmd = HostToBridge::SpikeLoad { profile: SpikeProfile::Burst, count: 300 };
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [13, 4, 0xAC, 0x02]);
    for (want, profile) in [
        SpikeProfile::Off,
        SpikeProfile::Drive,
        SpikeProfile::Fleet20,
        SpikeProfile::Saturate,
        SpikeProfile::Burst,
    ]
    .into_iter()
    .enumerate()
    {
        let n = encode_host_frame(&HostToBridge::SpikeLoad { profile, count: 0 }, &mut out)
            .expect("encodes");
        assert_eq!(frame_body(&out[..n])[1], want as u8, "{profile:?}");
    }
}

#[test]
fn link_codec_pins_spike_load_bytes_when_bridge_sends_a_record() {
    // Postcard's variant index, the seq as a varint, then the body as a byte string.
    let event =
        BridgeToHost::SpikeLoad { seq: 300, body: SpikeBody::from_slice(&[9, 8]).expect("fits") };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [12, 0xAC, 0x02, 2, 9, 8]);
}

#[test]
fn link_codec_pins_spike_stats_bytes_when_bridge_reports_them() {
    let stats = SpikeStats {
        seq: 1,
        uptime_ms: 300,
        ble_owned_ms: 200,
        queued_bytes: 2,
        sent_bytes: 3,
        credited_bytes: 4,
        delivered_bytes: 5,
        bulk_high_water: 6,
        refused: 7,
        evicted: 8,
        mtu: 9,
        phy: 10,
        interval_us: 11,
        radio_link: RadioLinkStats { packets: 12, ..RadioLinkStats::default() },
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&BridgeToHost::SpikeStats(stats), &mut out).expect("encodes");
    #[rustfmt::skip]
    let expected = [
        13,                         // BridgeToHost::SpikeStats
        1,                          // seq
        0xAC, 0x02,                 // uptime_ms 300, varint
        0xC8, 0x01,                 // ble_owned_ms 200, varint
        2, 3, 4, 5, 6, 7, 8,        // queued_bytes to evicted
        9, 10, 11,                  // mtu, phy, interval_us
        12, 0, 0, 0, 0, 0, 0, 0, 0, // radio_link
    ];
    assert_eq!(frame_body(&out[..n]), expected);
}

#[test]
fn link_round_trips_spike_load_when_body_is_full() {
    let mut body = [0u8; FULL_BODY];
    fill_body(u32::MAX, &mut body);
    let event = BridgeToHost::SpikeLoad {
        seq: u32::MAX,
        body: SpikeBody::from_slice(&body).expect("fits"),
    };
    let decoded = round_trip(&event);
    assert_eq!(decoded, event);
    let BridgeToHost::SpikeLoad { body, .. } = decoded else { unreachable!() };
    assert!(body_checks(&body));
}

#[test]
fn link_round_trips_spike_stats_when_every_field_is_largest() {
    let event = BridgeToHost::SpikeStats(full_spike_stats());
    assert_eq!(round_trip(&event), event);
}
