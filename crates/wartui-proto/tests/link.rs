//! The host-to-bridge USB framing.
//!
//! Both sample sets carry one case per variant and panic rather than `ok()` on a full
//! vector: a push that failed silently would drop a variant out of every round trip
//! below and leave the suite passing.

use heapless::{String, Vec};
use wartui_proto::air::{
    Board, Capabilities, DieTemp, HeartbeatMsg, RecordKind, Security, SightingBatchWriter,
    SightingMsg, Vdd,
};
use wartui_proto::air::{OtaState, OtaStatusMsg};
use wartui_proto::link::{
    BROADCAST, BURST_BUDGET_US, BURST_GAP_US, BridgeToHost, Chip, EspNowPayload, FrameAccumulator,
    GPS_FRAME, GPS_WRITE_MAX, GpsBytes, GpsWriteBytes, HostToBridge, LINK_PROTO_VERSION, LinkBurst,
    LinkError, LogLevel, LoopPhase, MAX_ESPNOW_V2_PAYLOAD, MAX_FRAME, MAX_HOST_FRAME,
    MAX_NODE_PAYLOAD, PANEL_ROWS, Panel, PanelLine, PanelLines, QueueId, QueueStat, QueueStats,
    RadioInfo, SendPayload, SendStatus, Severity, ShortStr, SpiStats, UPDATE_STATUS_MAX, UpdateOp,
    UpdateStatus, crc16, decode_frame, encode_frame, encode_host_frame,
};
use wartui_proto::reset::ResetCause;
use wartui_proto::spi::update::{OP_MAX, Op};

fn sample_commands() -> Vec<HostToBridge, 16> {
    let mut payload = Vec::new();
    payload
        .extend_from_slice(
            &HeartbeatMsg {
                counter: 42,
                epoch: 0,
                capabilities: Capabilities::here(Board::Esp32c5),
                wifi_refused: 0,
                ble_refused: 0,
                beat: 1,
                unsent: 0,
                dwell: 0,
                prev_dwell: 0,
                tx_power: 8,
                reset_cause: ResetCause::PowerOn,
                vdd: Vdd::NONE,
                die_temp: DieTemp::NONE,
            }
            .encode(),
        )
        .expect("a heartbeat fits a send");

    let mut v = Vec::new();
    v.push(HostToBridge::AddPeer { mac: [1, 2, 3, 4, 5, 6] }).expect("sample_commands has room");
    v.push(HostToBridge::RemovePeer { mac: [1, 2, 3, 4, 5, 6] }).expect("sample_commands has room");
    v.push(HostToBridge::GetStatus).expect("sample_commands has room");
    v.push(HostToBridge::Reset).expect("sample_commands has room");
    v.push(HostToBridge::ShowPanel { lines: full_panel() }).expect("sample_commands has room");
    v.push(HostToBridge::SetTxPower { power: 8 }).expect("sample_commands has room");
    v.push(HostToBridge::SendEspNow { id: 0xBEEF, dst: [0xAA; 6], ensure_peer: true, payload })
        .expect("sample_commands has room");
    v.push(HostToBridge::GpsWrite {
        bytes: GpsWriteBytes::from_slice(&[0xB5, 0x62, 0x06, 0x8A]).expect("fits"),
    })
    .expect("sample_commands has room");
    v.push(HostToBridge::Update(encoded_op(&Op::Begin { session: 7, len: 1234, sha256: [9; 32] })))
        .expect("sample_commands has room");
    v
}

/// `op` as a [`HostToBridge::Update`] carries it.
fn encoded_op(op: &Op<'_>) -> UpdateOp {
    let mut buf = [0u8; OP_MAX];
    let len = op.encode(&mut buf);
    UpdateOp::from_slice(&buf[..len]).expect("an op fits")
}

/// A status as a [`BridgeToHost::Update`] carries it.
fn encoded_status(status: OtaStatusMsg) -> UpdateStatus {
    let mut buf = [0u8; UPDATE_STATUS_MAX];
    let len = Op::Status(status).encode(&mut buf);
    UpdateStatus::from_slice(&buf[..len]).expect("a status fits")
}

/// The postcard bytes between a frame's version byte and its CRC.
fn frame_body(frame: &[u8]) -> std::vec::Vec<u8> {
    let mut decoded = frame.to_vec();
    let len = cobs::decode_in_place(&mut decoded[..frame.len() - 1]).expect("valid cobs");
    decoded[1..len - 2].to_vec()
}

/// Every row full, which is the worst case the const assert in `link/frame.rs` is checked against.
fn full_panel() -> PanelLines {
    let levels = [Severity::Ok, Severity::Warn, Severity::Error];
    let mut lines = PanelLines::new();
    for row in 0..PANEL_ROWS {
        let text = ShortStr::try_from("x".repeat(32).as_str()).expect("32 is the capacity");
        lines.push(PanelLine { level: levels[row % levels.len()], text }).ok();
    }
    lines
}

fn sample_events() -> Vec<BridgeToHost, 16> {
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
    let mut payload = Vec::new();
    payload.extend_from_slice(writer.as_bytes()).expect("a batch fits the node ceiling");

    let mut v = Vec::new();
    v.push(BridgeToHost::Ready {
        chip: Chip::Esp32C6,
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x60],
        fw_version: String::try_from("0.1.0").expect("short"),
        proto_version: LINK_PROTO_VERSION,
        // Not the defaults: a round trip that only ever carried the zero
        // variant of an enum would pass just as well with the field dropped.
        reset_cause: ResetCause::Watchdog,
        last_phase: LoopPhase::TxStalled,
        heap_free: 61_234,
        uptime_ms: 8_675_309,
        panel: None,
        gps: false,
        radio: None,
        image: None,
    })
    .expect("sample_events has room");
    // A second one, because `Option` and the enums the first case does not reach are the
    // fields a round trip would otherwise pass on without carrying.
    v.push(BridgeToHost::Ready {
        chip: Chip::Esp32C5,
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x61],
        fw_version: String::try_from("0.1.0").expect("short"),
        proto_version: LINK_PROTO_VERSION,
        reset_cause: ResetCause::Lockup,
        last_phase: LoopPhase::Render,
        heap_free: 60_000,
        uptime_ms: 1_000,
        panel: Some(Panel { cols: 26, rows: 8 }),
        gps: true,
        radio: None,
        image: Some([0xD1; 32]),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Rx { src: [9; 6], dst: BROADCAST, rssi: -73, rx_us: 123_456, payload })
        .expect("sample_events has room");
    v.push(BridgeToHost::SendResult { id: 7, status: SendStatus::AckOk, tx_us: 999 })
        .expect("sample_events has room");
    v.push(BridgeToHost::Status {
        peer_count: 3,
        rx_count: 1000,
        dropped_tx: 0,
        uptime_ms: 60_000,
        rx_oversize: 2,
        queues: QueueStats::new(),
        spi: None,
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Log {
        level: LogLevel::Warn,
        message: String::try_from("outbound ring full").expect("short"),
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Gps { bytes: GpsBytes::from_slice(b"$GNGGA,").expect("fits") })
        .expect("sample_events has room");
    v.push(BridgeToHost::Update(encoded_status(OtaStatusMsg {
        session: 7,
        state: OtaState::Receiving,
        next_offset: 4096,
    })))
    .expect("sample_events has room");
    v
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
fn link_codec_fits_full_panel_display_when_encoding_show_panel_command() {
    // The const assert in `link/frame.rs` says this arithmetically; this says it through the
    // encoder, which is the thing that would actually truncate.
    let cmd = HostToBridge::ShowPanel { lines: full_panel() };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert!(n < MAX_FRAME, "a full panel used {n} of {MAX_FRAME} bytes");
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
    let cmd = HostToBridge::AddPeer { mac: [1, 2, 3, 4, 5, 6] };
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
    let cmd = HostToBridge::AddPeer { mac: [0xa, 0xb, 0xc, 0xd, 0xe, 0xf] };
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
fn frame_accumulator_reports_empty_when_frame_completes() {
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    assert!(acc.is_empty());
    let mut out = [0u8; MAX_FRAME];
    let n = encode_host_frame(&HostToBridge::GetStatus, &mut out).expect("encodes");
    for &byte in &out[..n - 1] {
        assert!(acc.push(byte).is_none());
        assert!(!acc.is_empty(), "a partial frame reads as empty");
    }
    assert!(acc.push(0).is_some());
    assert!(acc.is_empty());
}

#[test]
fn frame_accumulator_reports_not_empty_when_overflowing_frame_awaits_terminator() {
    // An overlong frame is still arriving until its terminator, whatever the
    // accumulator kept of it.
    let mut acc = FrameAccumulator::<64>::new();
    for _ in 0..200 {
        acc.push(0xAB);
    }
    assert!(!acc.is_empty());
    assert!(acc.push(0).is_none());
    assert!(acc.is_empty());
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
fn host_command_asks_for_reply_only_when_bridge_answers() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&[0; 8]).expect("8 fits a send");
    let mac = [1, 2, 3, 4, 5, 6];
    let cases: [(HostToBridge, bool); 10] = [
        (HostToBridge::Identify, true),
        (HostToBridge::GetStatus, true),
        (HostToBridge::SendEspNow { id: 1, dst: mac, ensure_peer: false, payload }, true),
        (HostToBridge::ShowPanel { lines: full_panel() }, false),
        (HostToBridge::SetTxPower { power: 8 }, false),
        (HostToBridge::AddPeer { mac }, false),
        (HostToBridge::RemovePeer { mac }, false),
        (HostToBridge::Reset, false),
        (HostToBridge::GpsWrite { bytes: GpsWriteBytes::new() }, false),
        (HostToBridge::Update(encoded_op(&Op::Commit { session: 1 })), false),
    ];
    for (command, asks) in cases {
        assert_eq!(command.asks_for_reply(), asks, "{command:?}");
    }
}

#[test]
fn reset_cause_speaks_first_when_a_host_was_present_unless_power_on() {
    for cause in [
        ResetCause::PowerOn,
        ResetCause::Software,
        ResetCause::External,
        ResetCause::Brownout,
        ResetCause::Watchdog,
        ResetCause::Lockup,
        ResetCause::Unknown,
    ] {
        let after_host = !matches!(cause, ResetCause::PowerOn);
        assert_eq!(cause.speaks_first(true), after_host, "{cause:?}");
        assert!(!cause.speaks_first(false), "{cause:?}");
    }
}

#[test]
fn link_round_trips_v2_send_when_payload_is_full() {
    let payload = SendPayload::from_slice(&[0xA5; MAX_ESPNOW_V2_PAYLOAD]).expect("fits");
    let cmd = HostToBridge::SendEspNow { id: 7, dst: [1; 6], ensure_peer: true, payload };
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
fn link_round_trips_node_rx_when_payload_is_full_and_has_no_zeros() {
    // COBS grows a frame by one byte per 254 without a zero, so a full node frame with
    // none is the longest bridge-to-host frame there is.
    let payload = EspNowPayload::from_slice(&[0xFF; MAX_NODE_PAYLOAD]).expect("fits");
    let event = BridgeToHost::Rx { src: [9; 6], dst: [2; 6], rssi: -40, rx_us: u32::MAX, payload };
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
fn link_refuses_send_payload_when_longer_than_v2_ceiling() {
    assert!(SendPayload::from_slice(&[0; MAX_ESPNOW_V2_PAYLOAD + 1]).is_err());
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
        peer_count: 1,
        rx_count: 10,
        dropped_tx: 0,
        uptime_ms: 5_000,
        rx_oversize: 0xDEAD_BEEF,
        queues: QueueStats::new(),
        spi: None,
    };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&status, &mut out).expect("encodes");
    let decoded: BridgeToHost = decode_frame(&mut out[..n - 1]).expect("decodes");
    assert_eq!(decoded, status);
}

#[test]
fn link_burst_stops_when_no_frame_is_mid_transfer() {
    let mut burst = LinkBurst::start(0);
    assert!(!burst.keep_going(1, true, false));
}

#[test]
fn link_burst_continues_when_frame_is_mid_transfer_at_start() {
    // The first poll has moved nothing yet, and starting counts as a move.
    let mut burst = LinkBurst::start(0);
    assert!(burst.keep_going(0, false, true));
}

#[test]
fn link_burst_stops_when_nothing_moves_for_the_gap() {
    // A host that stopped reading mid-frame: nothing moves after the start.
    let mut burst = LinkBurst::start(0);
    assert!(burst.keep_going(BURST_GAP_US - 1, false, true));
    assert!(!burst.keep_going(BURST_GAP_US, false, true));
}

#[test]
fn link_burst_continues_when_bytes_keep_moving() {
    // A packet every 150 µs, polled every 10 µs in between: inside the gap, so the
    // burst runs until the budget.
    let mut burst = LinkBurst::start(0);
    for now in (0..BURST_BUDGET_US).step_by(10) {
        assert!(burst.keep_going(now, now % 150 == 0, true), "stopped {now} µs in");
    }
}

#[test]
fn link_burst_times_gap_from_last_move_when_bytes_moved_late() {
    let mut burst = LinkBurst::start(0);
    assert!(burst.keep_going(150, true, true));
    assert!(burst.keep_going(150 + BURST_GAP_US - 1, false, true));
    assert!(!burst.keep_going(150 + BURST_GAP_US, false, true));
}

#[test]
fn link_burst_stops_when_budget_is_spent_despite_progress() {
    // A host that never stops sending: the radio still gets the pass back.
    let mut burst = LinkBurst::start(0);
    assert!(burst.keep_going(BURST_BUDGET_US - 1, true, true));
    assert!(!burst.keep_going(BURST_BUDGET_US, true, true));
}

#[test]
fn link_burst_times_budget_from_start_when_clock_starts_late() {
    let start = 5_000_000;
    let mut burst = LinkBurst::start(start);
    assert!(burst.keep_going(start + BURST_BUDGET_US - 1, true, true));
    assert!(!burst.keep_going(start + BURST_BUDGET_US, true, true));
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
            QueueId::RadioSendJobs => 1,
            QueueId::RadioDown => 2,
            QueueId::HostPriority => 3,
            QueueId::HostBulk => 4,
            QueueId::RadioUpPriority => 5,
            QueueId::RadioJobs => 6,
        };
        assert_eq!(index, declared, "{id:?}");
    }
}

#[test]
fn queue_id_round_trips_name_when_every_id_named() {
    for id in QueueId::ALL {
        assert_eq!(QueueId::from_name(id.name()), Some(id), "{id:?}");
    }
    assert_eq!(QueueId::from_name("host"), None);
}

#[test]
fn link_round_trips_status_with_queues_and_spi() {
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
        peer_count: 1,
        rx_count: 10,
        dropped_tx: 0,
        uptime_ms: 5_000,
        rx_oversize: 0,
        queues,
        spi: Some(SpiStats {
            transfers: u32::MAX,
            crc_fail: 1,
            bad_magic: 2,
            seq_gap: 3,
            ready_timeouts: 4,
            ready_recovered: 5,
            radio_crc_fail: 6,
            radio_bad_magic: 7,
            radio_seq_gap: 8,
        }),
    };
    assert_eq!(round_trip(&status), status);
}

#[test]
fn link_round_trips_ready_with_radio() {
    let ready = BridgeToHost::Ready {
        chip: Chip::Esp32P4,
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x62],
        fw_version: String::try_from("0.1.0").expect("short"),
        proto_version: LINK_PROTO_VERSION,
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::SpiTransfer,
        heap_free: 1,
        uptime_ms: 2,
        panel: None,
        gps: false,
        radio: Some(RadioInfo {
            chip: Chip::Esp32C6,
            mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x63],
            fw_version: String::try_from("0.1.1").expect("short"),
            reset_cause: ResetCause::Watchdog,
            last_phase: LoopPhase::Transmit,
            uptime_ms: u32::MAX,
        }),
        image: Some([0xA5; 32]),
    };
    assert_eq!(round_trip(&ready), ready);
}

#[test]
fn link_codec_pins_update_bytes_when_host_sends_an_op() {
    // Postcard's variant index, then the op as a byte string: its length, then the
    // `spi::update` layout. The index is `Update`'s place at the end of `HostToBridge`.
    let cmd = HostToBridge::Update(encoded_op(&Op::Commit { session: 0x42 }));
    let mut out = [0u8; MAX_HOST_FRAME];
    let n = encode_host_frame(&cmd, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [9, 2, 3, 0x42]);
}

#[test]
fn link_codec_pins_update_bytes_when_bridge_sends_a_status() {
    let status =
        OtaStatusMsg { session: 0x42, state: OtaState::Verified, next_offset: 0x0001_2000 };
    let event = BridgeToHost::Update(encoded_status(status));
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&event, &mut out).expect("encodes");
    assert_eq!(frame_body(&out[..n]), [7, 7, 5, 0x42, 2, 0x00, 0x20, 0x01, 0x00]);
}

#[test]
fn link_round_trips_update_when_op_is_a_full_chunk() {
    let data = [0xFF; wartui_proto::air::OTA_CHUNK];
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
            chip: Chip::Esp32P4,
            mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x62],
            fw_version: String::try_from("0.1.0").expect("short"),
            proto_version: LINK_PROTO_VERSION,
            reset_cause: ResetCause::Software,
            last_phase: LoopPhase::Boot,
            heap_free: 0,
            uptime_ms: 3,
            panel: None,
            gps: true,
            radio: None,
            image,
        };
        assert_eq!(round_trip(&ready), ready);
    }
}
