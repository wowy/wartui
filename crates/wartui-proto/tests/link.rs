//! The host-to-bridge USB framing.
//!
//! Both sample sets carry one case per variant and panic rather than `ok()` on a full
//! vector: a push that failed silently would drop a variant out of every round trip
//! below and leave the suite passing.

use heapless::{String, Vec};
use wartui_proto::air::{
    Capabilities, HeartbeatMsg, RecordKind, SIGHTING_MSG_MAX, Security, SightingMsg,
};
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LinkError,
    LogLevel, LoopPhase, MAX_FRAME, PANEL_ROWS, Panel, PanelLine, PanelLines, ResetCause,
    SendStatus, Severity, ShortStr, crc16, decode_frame, encode_frame,
};

fn sample_commands() -> Vec<HostToBridge, 16> {
    let mut payload = Vec::new();
    payload
        .extend_from_slice(
            &HeartbeatMsg { counter: 42, capabilities: Capabilities::here(true, true) }.encode(),
        )
        .expect("212 fits in 250");

    let mut v = Vec::new();
    v.push(HostToBridge::SetChannel { channel: 6 }).expect("sample_commands has room");
    v.push(HostToBridge::AddPeer { mac: [1, 2, 3, 4, 5, 6] }).expect("sample_commands has room");
    v.push(HostToBridge::RemovePeer { mac: [1, 2, 3, 4, 5, 6] }).expect("sample_commands has room");
    v.push(HostToBridge::GetStatus).expect("sample_commands has room");
    v.push(HostToBridge::Reset).expect("sample_commands has room");
    v.push(HostToBridge::ShowPanel { lines: full_panel() }).expect("sample_commands has room");
    v.push(HostToBridge::SetTxPower { power: 8 }).expect("sample_commands has room");
    // Last on purpose: `a_full_212_byte_frame_fits_with_room_to_spare` takes the final case
    // and measures it as the ESP-NOW one. Anything pushed after this silently becomes the
    // frame that test believes it is sizing.
    v.push(HostToBridge::SendEspNow { id: 0xBEEF, dst: [0xAA; 6], ensure_peer: true, payload })
        .expect("sample_commands has room");
    v
}

/// Every row full, which is the worst case the const assert in `link.rs` is checked against.
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
    let mut frame = [0u8; SIGHTING_MSG_MAX];
    let len = SightingMsg {
        kind: RecordKind::Wifi,
        bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        channel: 6,
        rssi: -50,
        security: Security::Wpa2Psk,
        ssid: b"net",
        ext: &[],
    }
    .encode_into(&mut frame)
    .expect("fits");
    let mut payload = Vec::new();
    payload.extend_from_slice(&frame[..len]).expect("a sighting fits in 250");

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
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Rx {
        src: [9; 6],
        dst: BROADCAST,
        rssi: -73,
        channel: 6,
        rx_us: 123_456,
        payload,
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::SendResult { id: 7, status: SendStatus::AckOk, tx_us: 999 })
        .expect("sample_events has room");
    v.push(BridgeToHost::Status {
        channel: 6,
        peer_count: 3,
        rx_count: 1000,
        dropped_tx: 0,
        uptime_ms: 60_000,
    })
    .expect("sample_events has room");
    v.push(BridgeToHost::Log {
        level: LogLevel::Warn,
        message: String::try_from("outbound ring full").expect("short"),
    })
    .expect("sample_events has room");
    v
}

#[test]
fn crc16_matches_ccitt_false_standard_when_given_test_vectors() {
    // The standard check value for CRC-16/CCITT-FALSE.
    assert_eq!(crc16(b"123456789"), 0x29B1);
    assert_eq!(crc16(b""), 0xFFFF);
}

#[test]
fn link_codec_round_trips_all_host_commands_when_encoded_and_decoded() {
    for cmd in sample_commands() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(&cmd, &mut out).expect("encodes");
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
fn link_codec_fits_max_esp_now_payload_when_encoding_frame() {
    let cmd = sample_commands().into_iter().next_back().expect("the SendEspNow case");
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&cmd, &mut out).expect("encodes");
    assert!(n < MAX_FRAME, "a full ESP-NOW frame used {n} of {MAX_FRAME} bytes");
}

#[test]
fn link_codec_fits_full_panel_display_when_encoding_show_panel_command() {
    // The const assert in `link.rs` says this arithmetically; this says it through the
    // encoder, which is the thing that would actually truncate.
    let cmd = HostToBridge::ShowPanel { lines: full_panel() };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&cmd, &mut out).expect("encodes");
    assert!(n < MAX_FRAME, "a full panel used {n} of {MAX_FRAME} bytes");
}

#[test]
fn link_codec_terminates_frame_with_single_null_byte_when_cobs_encoded() {
    // This is what makes the terminator unambiguous and resync possible.
    for cmd in sample_commands() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(&cmd, &mut out).expect("encodes");
        assert_eq!(out[n - 1], 0, "frame must end with the terminator");
        assert!(!out[..n - 1].contains(&0), "COBS body must not contain a zero");
    }
}

#[test]
fn link_decoder_rejects_frame_when_payload_checksum_is_corrupted() {
    let cmd = HostToBridge::SetChannel { channel: 6 };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&cmd, &mut out).expect("encodes");

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
    let n = encode_frame(&cmd, &mut out).expect("encodes");

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
        let n = encode_frame(cmd, &mut out).expect("encodes");
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
fn frame_accumulator_resynchronises_cleanly_when_stream_contains_bootloader_noise() {
    // A bridge reset sprays ROM bootloader chatter down the same pipe before
    // the first real frame. Nothing before the next terminator should survive,
    // and everything after it should.
    let cmd = HostToBridge::SetChannel { channel: 11 };
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&cmd, &mut out).expect("encodes");

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
    let n = encode_frame(&cmd, &mut out).expect("encodes");
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
    let n = encode_frame(&cmd, &mut out).expect("encodes");
    let mut got = None;
    for byte in out[..n].iter().copied() {
        if let Some(frame) = acc.push(byte) {
            got = Some(decode_frame::<HostToBridge>(frame).expect("decodes"));
        }
    }
    assert_eq!(got, Some(cmd));
}

#[test]
fn link_decoder_rejects_payload_when_frame_is_truncated() {
    let mut empty: [u8; 0] = [];
    assert!(decode_frame::<HostToBridge>(&mut empty).is_err());
    let mut tiny = [1u8, 2];
    assert!(decode_frame::<HostToBridge>(&mut tiny).is_err());
}
