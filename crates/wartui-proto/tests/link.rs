//! The host-to-bridge USB framing.

use heapless::{String, Vec};
use wartui_proto::air::{MsgType, TextMsg};
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LinkError,
    LogLevel, MAX_FRAME, SendStatus, crc16, decode_frame, encode_frame,
};

fn sample_commands() -> Vec<HostToBridge, 8> {
    let mut payload = Vec::new();
    payload
        .extend_from_slice(&TextMsg::new(MsgType::Heartbeat, 42, b"").expect("fits").encode())
        .expect("212 fits in 250");

    let mut v = Vec::new();
    v.push(HostToBridge::SetChannel { channel: 6 }).ok();
    v.push(HostToBridge::AddPeer { mac: [1, 2, 3, 4, 5, 6] }).ok();
    v.push(HostToBridge::RemovePeer { mac: [1, 2, 3, 4, 5, 6] }).ok();
    v.push(HostToBridge::GetStatus).ok();
    v.push(HostToBridge::Reset).ok();
    v.push(HostToBridge::SendEspNow { id: 0xBEEF, dst: [0xAA; 6], ensure_peer: true, payload })
        .ok();
    v
}

fn sample_events() -> Vec<BridgeToHost, 8> {
    let mut payload = Vec::new();
    payload
        .extend_from_slice(
            &TextMsg::new(MsgType::Text, 0, b"AA:BB:CC:DD:EE:FF,net,[WPA2_PSK],6,-50,W")
                .expect("fits")
                .encode(),
        )
        .expect("212 fits in 250");

    let mut v = Vec::new();
    v.push(BridgeToHost::Ready {
        chip: Chip::Esp32C6,
        mac: [0x10, 0x20, 0x30, 0x40, 0x50, 0x60],
        fw_version: String::try_from("0.1.0").expect("short"),
        proto_version: LINK_PROTO_VERSION,
    })
    .ok();
    v.push(BridgeToHost::Rx {
        src: [9; 6],
        dst: BROADCAST,
        rssi: -73,
        channel: 6,
        rx_us: 123_456,
        payload,
    })
    .ok();
    v.push(BridgeToHost::SendResult { id: 7, status: SendStatus::AckOk, tx_us: 999 }).ok();
    v.push(BridgeToHost::Status {
        channel: 6,
        peer_count: 3,
        rx_count: 1000,
        dropped_tx: 0,
        uptime_ms: 60_000,
    })
    .ok();
    v.push(BridgeToHost::Log {
        level: LogLevel::Warn,
        message: String::try_from("outbound ring full").expect("short"),
    })
    .ok();
    v
}

#[test]
fn crc16_matches_the_ccitt_false_check_value() {
    // The standard check value for CRC-16/CCITT-FALSE.
    assert_eq!(crc16(b"123456789"), 0x29B1);
    assert_eq!(crc16(b""), 0xFFFF);
}

#[test]
fn commands_round_trip() {
    for cmd in sample_commands() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(&cmd, &mut out).expect("encodes");
        let decoded: HostToBridge =
            decode_frame(&mut out[..n - 1]).expect("decodes without the terminator");
        assert_eq!(decoded, cmd);
    }
}

#[test]
fn events_round_trip() {
    for event in sample_events() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(&event, &mut out).expect("encodes");
        let decoded: BridgeToHost = decode_frame(&mut out[..n - 1]).expect("decodes");
        assert_eq!(decoded, event);
    }
}

#[test]
fn a_full_212_byte_frame_fits_with_room_to_spare() {
    let cmd = sample_commands().into_iter().next_back().expect("the SendEspNow case");
    let mut out = [0u8; MAX_FRAME];
    let n = encode_frame(&cmd, &mut out).expect("encodes");
    assert!(n < MAX_FRAME, "a full ESP-NOW frame used {n} of {MAX_FRAME} bytes");
}

#[test]
fn encoded_frames_have_exactly_one_zero_and_it_is_last() {
    // This is what makes the terminator unambiguous and resync possible.
    for cmd in sample_commands() {
        let mut out = [0u8; MAX_FRAME];
        let n = encode_frame(&cmd, &mut out).expect("encodes");
        assert_eq!(out[n - 1], 0, "frame must end with the terminator");
        assert!(!out[..n - 1].contains(&0), "COBS body must not contain a zero");
    }
}

#[test]
fn a_corrupted_byte_is_caught_by_the_checksum() {
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
fn a_frame_from_a_different_protocol_version_is_rejected_clearly() {
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
fn accumulator_yields_back_to_back_frames() {
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
fn accumulator_resynchronises_after_a_reset_banner() {
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
fn accumulator_ignores_runs_of_zeros() {
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
fn accumulator_discards_a_frame_too_large_to_be_ours() {
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
fn a_truncated_frame_is_rejected() {
    let mut empty: [u8; 0] = [];
    assert!(decode_frame::<HostToBridge>(&mut empty).is_err());
    let mut tiny = [1u8, 2];
    assert!(decode_frame::<HostToBridge>(&mut tiny).is_err());
}
