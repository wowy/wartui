//! The P4–C6 UART link: its framing, its frozen hello, and the messages it carries.

use heapless::Vec;
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::link::{
    BridgeToHost, Chip, HostToBridge, LogLevel, LoopPhase, MAX_ESPNOW_V2_PAYLOAD, MAX_NODE_PAYLOAD,
    QueueId, QueueStat, SendStatus, ShortStr, crc16,
};
use wartui_proto::radio_link::{
    BODY_MAX, CORE_VERSION, CRC_LEN, Down, HEADER_LEN, HELLO_IMAGE_LEN, HELLO_LEN, Held, Hello,
    Kind, MAGIC, PAYLOAD_MAX, Packer, PacketReader, RECORD_MAX, RadioStatus, SeqCheck, SeqNote, Up,
    VerifyError, WIRE_MAX, decode_down, decode_up, encode_down, encode_packet, encode_up, records,
    seal, verify,
};
use wartui_proto::reset::ResetCause;

fn log(text: &str) -> Up {
    Up::Message(BridgeToHost::Log { level: LogLevel::Info, message: text.try_into().unwrap() })
}

fn largest_send() -> Down {
    Down::Command(HostToBridge::SendEspNow {
        id: 0xBEEF,
        dst: [0x12; 6],
        ensure_peer: true,
        payload: Vec::from_slice(&[0xA5; MAX_ESPNOW_V2_PAYLOAD]).unwrap(),
    })
}

/// A sealed `Hello` packet with a three-byte payload, for the tests that corrupt one.
fn sealed() -> std::vec::Vec<u8> {
    let mut body = vec![0u8; BODY_MAX];
    body[HEADER_LEN..HEADER_LEN + 3].copy_from_slice(&[1, 2, 3]);
    let len = seal(&mut body, Kind::Hello, 7, 3);
    body.truncate(len);
    body
}

/// `body` as it goes on the wire.
fn wire(body: &[u8]) -> std::vec::Vec<u8> {
    let mut out = vec![0u8; WIRE_MAX];
    let len = encode_packet(body, &mut out);
    out.truncate(len);
    out
}

/// A body holding `len` payload bytes of `fill`, numbered `seq`.
fn body_of(fill: u8, seq: u32, len: usize) -> std::vec::Vec<u8> {
    let mut body = vec![0u8; BODY_MAX];
    body[HEADER_LEN..HEADER_LEN + len].fill(fill);
    let total = seal(&mut body, Kind::Messages, seq, len);
    body.truncate(total);
    body
}

/// What a reader makes of `bytes`: the seq and payload of each good packet, or why it refused.
fn read_all(
    reader: &mut PacketReader,
    bytes: &[u8],
) -> std::vec::Vec<Result<(u32, std::vec::Vec<u8>), VerifyError>> {
    let mut read = std::vec::Vec::new();
    for &byte in bytes {
        if let Some(packet) = reader.push(byte) {
            read.push(packet.map(|packet| (packet.seq, packet.payload.to_vec())));
        }
    }
    read
}

/// Packs one packet from `ring` through `held` and returns the records it carries.
fn pack_packet<const N: usize>(
    held: &mut Held,
    ring: &mut FrameRing<N>,
) -> std::vec::Vec<std::vec::Vec<u8>> {
    let mut payload = [0u8; PAYLOAD_MAX];
    let mut packer = Packer::new(&mut payload);
    held.pack(&mut packer, |out| ring.read(out).0);
    let len = packer.len();
    records(&payload[..len]).map(<[u8]>::to_vec).collect()
}

fn rewrite_crc(body: &mut [u8]) {
    let crc_at = body.len() - 2;
    let crc = crc16(&body[..crc_at]);
    body[crc_at..].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn radio_link_round_trips_messages_packet() {
    let first = log("first");
    let second = log("second");
    let mut body = [0xAAu8; BODY_MAX];
    let mut record = [0u8; 256];
    let mut packer = Packer::new(&mut body[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX]);
    for msg in [&first, &second] {
        let len = encode_up(msg, &mut record).unwrap();
        assert!(packer.push_encoded(&record[..len]));
    }
    let len = packer.len();
    let total = seal(&mut body, Kind::Messages, 0x0102_0304, len);
    assert_eq!(total, HEADER_LEN + len + CRC_LEN);

    let packet = verify(&body[..total]).unwrap();
    assert_eq!(packet.kind, Kind::Messages);
    assert_eq!(packet.seq, 0x0102_0304);
    assert_eq!(packet.payload.len(), len);
    let decoded: std::vec::Vec<Up> =
        records(packet.payload).map(|r| decode_up(r).unwrap()).collect();
    assert_eq!(decoded, [first, second]);
}

#[test]
fn radio_link_packet_round_trips_when_payload_is_empty_typical_or_full() {
    for len in [0, 300, PAYLOAD_MAX] {
        for fill in [0x00, 0x5A] {
            let body = body_of(fill, 42, len);
            let bytes = wire(&body);
            assert!(bytes.len() <= WIRE_MAX, "{len} bytes of {fill} took {}", bytes.len());
            // The terminator ends the packet, and is the only zero on the wire.
            assert_eq!(bytes.iter().position(|&b| b == 0), Some(bytes.len() - 1));
            let mut reader = PacketReader::new();
            assert_eq!(read_all(&mut reader, &bytes), [Ok((42, vec![fill; len]))]);
        }
    }
}

#[test]
fn radio_link_wire_max_holds_largest_body_when_it_has_no_zeros() {
    assert_eq!(wire(&vec![0x5Au8; BODY_MAX]).len(), WIRE_MAX);
}

#[test]
fn radio_link_reader_takes_packets_when_they_share_a_read() {
    let mut bytes = wire(&body_of(1, 10, 5));
    bytes.extend_from_slice(&wire(&body_of(2, 11, 6)));
    let mut reader = PacketReader::new();
    assert_eq!(read_all(&mut reader, &bytes), [Ok((10, vec![1; 5])), Ok((11, vec![2; 6]))]);
}

#[test]
fn radio_link_reader_takes_packet_when_it_spans_reads() {
    let bytes = wire(&body_of(3, 12, 500));
    let (first, second) = bytes.split_at(100);
    let mut reader = PacketReader::new();
    assert!(read_all(&mut reader, first).is_empty());
    assert_eq!(read_all(&mut reader, second), [Ok((12, vec![3; 500]))]);
}

#[test]
fn radio_link_reader_resyncs_when_garbage_precedes_packet() {
    // A boot's line noise, ended by a zero: it costs nothing but its own count.
    let mut bytes = std::vec::Vec::from([0x12, 0x34, 0x56, 0x00]);
    bytes.extend_from_slice(&wire(&body_of(4, 20, 8)));
    let mut reader = PacketReader::new();
    let read = read_all(&mut reader, &bytes);
    assert!(read[0].is_err(), "{read:?}");
    assert_eq!(read[1..], [Ok((20, vec![4; 8]))]);
}

#[test]
fn radio_link_reader_resyncs_when_garbage_has_no_terminator() {
    // The garbage runs into the first packet and spoils it; the zero that ends it realigns.
    let mut bytes = std::vec::Vec::from([0x12, 0x34, 0x56]);
    bytes.extend_from_slice(&wire(&body_of(4, 20, 8)));
    bytes.extend_from_slice(&wire(&body_of(5, 21, 8)));
    let mut reader = PacketReader::new();
    let read = read_all(&mut reader, &bytes);
    assert_eq!(read.len(), 2, "{read:?}");
    assert!(read[0].is_err(), "{read:?}");
    assert_eq!(read[1], Ok((21, vec![5; 8])));
}

#[test]
fn radio_link_reader_resyncs_when_packet_is_truncated() {
    let cut = wire(&body_of(6, 30, 400));
    let mut bytes = cut[..200].to_vec();
    bytes.extend_from_slice(&wire(&body_of(7, 31, 8)));
    bytes.extend_from_slice(&wire(&body_of(8, 32, 8)));
    let mut reader = PacketReader::new();
    let read = read_all(&mut reader, &bytes);
    assert_eq!(read.len(), 2, "{read:?}");
    assert!(read[0].is_err(), "{read:?}");
    assert_eq!(read[1], Ok((32, vec![8; 8])));
}

#[test]
fn radio_link_reader_reports_bad_crc_when_packet_is_oversize() {
    let mut bytes = vec![0x5Au8; WIRE_MAX * 2];
    bytes.push(0);
    bytes.extend_from_slice(&wire(&body_of(9, 40, 8)));
    let mut reader = PacketReader::new();
    assert_eq!(read_all(&mut reader, &bytes), [Err(VerifyError::BadCrc), Ok((40, vec![9; 8]))]);
}

#[test]
fn radio_link_reader_reports_bad_crc_when_cobs_is_invalid() {
    // A code byte promising more than the frame holds.
    let mut reader = PacketReader::new();
    assert_eq!(read_all(&mut reader, &[0x20, 1, 2, 0]), [Err(VerifyError::BadCrc)]);
}

#[test]
fn radio_link_reader_reports_nothing_when_only_zeros_arrive() {
    let mut reader = PacketReader::new();
    assert!(read_all(&mut reader, &[0, 0, 0]).is_empty());
}

#[test]
fn radio_link_rejects_flipped_byte_with_bad_crc() {
    let mut body = sealed();
    body[13] ^= 0x01;
    assert_eq!(verify(&body).unwrap_err(), VerifyError::BadCrc);
}

#[test]
fn radio_link_rejects_with_bad_crc_when_body_is_too_short() {
    let body = sealed();
    assert_eq!(verify(&body[..HEADER_LEN + 1]).unwrap_err(), VerifyError::BadCrc);
    assert_eq!(verify(&[]).unwrap_err(), VerifyError::BadCrc);
}

#[test]
fn radio_link_rejects_bad_magic_when_magic_flipped() {
    let mut body = sealed();
    body[0] ^= 0x01;
    assert_eq!(verify(&body).unwrap_err(), VerifyError::BadMagic);
}

#[test]
fn radio_link_rejects_other_core_version() {
    let mut body = sealed();
    body[2] = 2;
    rewrite_crc(&mut body);
    assert_eq!(verify(&body).unwrap_err(), VerifyError::OtherCore(2));
}

#[test]
fn radio_link_rejects_bad_len_when_len_past_payload_max() {
    let mut body = sealed();
    let len = u16::try_from(PAYLOAD_MAX + 1).unwrap();
    body[8..10].copy_from_slice(&len.to_le_bytes());
    rewrite_crc(&mut body);
    assert_eq!(verify(&body).unwrap_err(), VerifyError::BadLen);
}

#[test]
fn radio_link_rejects_bad_len_when_len_is_not_the_payload_length() {
    let mut body = sealed();
    body[8] = 2;
    rewrite_crc(&mut body);
    assert_eq!(verify(&body).unwrap_err(), VerifyError::BadLen);
}

#[test]
fn radio_link_rejects_unknown_kind_when_kind_unassigned() {
    let mut body = sealed();
    body[3] = 9;
    rewrite_crc(&mut body);
    assert_eq!(verify(&body).unwrap_err(), VerifyError::UnknownKind(9));
}

#[test]
fn radio_link_rejects_unknown_kind_when_kind_is_zero() {
    // Code 0 names nothing: a side with nothing to say sends nothing.
    let mut body = sealed();
    body[3] = 0;
    rewrite_crc(&mut body);
    assert_eq!(verify(&body).unwrap_err(), VerifyError::UnknownKind(0));
}

#[test]
fn radio_link_header_has_frozen_layout() {
    let mut body = [0xAAu8; BODY_MAX];
    body[HEADER_LEN..HEADER_LEN + 3].copy_from_slice(&[1, 2, 3]);
    let len = seal(&mut body, Kind::Hello, 0x0A0B_0C0D, 3);
    assert_eq!(len, HEADER_LEN + 3 + CRC_LEN);
    // MAGIC "WT", core 1, kind 1, seq LE, len 3, reserved zero.
    assert_eq!(body[..HEADER_LEN], [0x54, 0x57, 1, 1, 0x0D, 0x0C, 0x0B, 0x0A, 3, 0, 0, 0]);
    assert_eq!((MAGIC, CORE_VERSION, BODY_MAX, PAYLOAD_MAX), (0x5754, 1, 2048, 2034));
    assert_eq!(WIRE_MAX, 2058);
    // CRC-16/CCITT-FALSE 0x8A20, little-endian, over the header and the three payload bytes,
    // directly after them. Worked out with a separate bitwise CRC, checked against the standard
    // "123456789" value 0x29B1.
    assert_eq!(body[HEADER_LEN + 3..len], [0x20, 0x8A]);
    // Nothing past the CRC is touched.
    assert_eq!(body[len], 0xAA);
    // The packet on the wire, by hand: each code byte counts to the next zero it stands for.
    let mut out = [0u8; WIRE_MAX];
    let wire_len = encode_packet(&body[..len], &mut out);
    #[rustfmt::skip]
    let expected = [
        10, 0x54, 0x57, 1, 1, 0x0D, 0x0C, 0x0B, 0x0A, 3, // up to the reserved bytes
        1, 1,                                            // the first two reserved bytes
        6, 1, 2, 3, 0x20, 0x8A,                          // the payload and the CRC
        0,                                               // the terminator
    ];
    assert_eq!(out[..wire_len], expected);
}

/// These bytes are the frozen hello. They must never change: any P4 build reads any C6
/// build's hello by this layout. Written out by hand from the layout table in
/// `radio_link/hello.rs`, not captured from the encoder.
#[test]
fn radio_link_hello_has_frozen_layout() {
    let hello = Hello {
        link_rev: 0x0102,
        chip: Chip::Esp32C6,
        mac: [0xD8, 0x3B, 0xDA, 0x10, 0xD8, 0x1C],
        fw_version: ShortStr::try_from("0.9.1").unwrap(),
        reset_cause: ResetCause::Watchdog,
        last_phase: LoopPhase::Pump,
        uptime_ms: 0x1122_3344,
        heap_free: 0x0005_6789,
        image: None,
    };
    let mut out = [0xFFu8; 64];
    assert_eq!(hello.encode(&mut out), HELLO_LEN);
    #[rustfmt::skip]
    let expected: [u8; 51] = [
        // 0: link_rev, u16 LE
        0x02, 0x01,
        // 2: chip, Esp32C6 is index 1
        0x01,
        // 3: mac
        0xD8, 0x3B, 0xDA, 0x10, 0xD8, 0x1C,
        // 9: fw_version, "0.9.1" then zeros to 32 bytes
        b'0', b'.', b'9', b'.', b'1', 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        // 41: reset_cause, Watchdog is index 2
        0x02,
        // 42: last_phase, Pump is frozen code 6, whatever its enum index
        0x06,
        // 43: uptime_ms, u32 LE
        0x44, 0x33, 0x22, 0x11,
        // 47: heap_free, u32 LE
        0x89, 0x67, 0x05, 0x00,
    ];
    assert_eq!(out[..HELLO_LEN], expected);
    assert_eq!(Hello::decode(&expected), Some(hello));
}

#[test]
fn radio_link_hello_round_trips_when_chip_is_p4() {
    let hello = Hello {
        link_rev: 1,
        chip: Chip::Esp32P4,
        mac: [1, 2, 3, 4, 5, 6],
        fw_version: ShortStr::try_from("0.1.0").unwrap(),
        reset_cause: ResetCause::Unknown,
        last_phase: LoopPhase::RadioLink,
        uptime_ms: 5,
        heap_free: 6,
        image: None,
    };
    let mut out = [0u8; HELLO_LEN];
    hello.encode(&mut out);
    assert_eq!(out[2], 2);
    assert_eq!(out[41], 6);
    assert_eq!(out[42], 10);
    assert_eq!(Hello::decode(&out), Some(hello));
}

#[test]
fn radio_link_hello_ignores_appended_bytes_when_payload_runs_past_image() {
    // A later build may append fields; this build reads the frozen 83 bytes and no more.
    let hello = Hello {
        link_rev: 1,
        chip: Chip::Esp32C6,
        mac: [1, 2, 3, 4, 5, 6],
        fw_version: ShortStr::try_from("0.1.0").unwrap(),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Boot,
        uptime_ms: 72,
        heap_free: 6,
        image: Some([0x5A; 32]),
    };
    let mut out = [0xEEu8; HELLO_IMAGE_LEN + 32];
    assert_eq!(hello.encode(&mut out), HELLO_IMAGE_LEN);
    assert_eq!(Hello::decode(&out), Some(hello));
}

/// The image digest is part of the frozen hello: bytes 51..83, appended to the layout
/// `radio_link_hello_has_frozen_layout` pins.
#[test]
fn radio_link_hello_has_frozen_layout_when_image_present() {
    let mut digest = [0u8; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = i as u8;
    }
    let hello = Hello {
        link_rev: 0x0102,
        chip: Chip::Esp32C6,
        mac: [0xD8, 0x3B, 0xDA, 0x10, 0xD8, 0x1C],
        fw_version: ShortStr::try_from("0.9.1").unwrap(),
        reset_cause: ResetCause::Watchdog,
        last_phase: LoopPhase::Pump,
        uptime_ms: 0x1122_3344,
        heap_free: 0x0005_6789,
        image: Some(digest),
    };
    let mut out = [0xFFu8; 96];
    assert_eq!(hello.encode(&mut out), HELLO_IMAGE_LEN);
    assert_eq!(HELLO_IMAGE_LEN, 83);
    #[rustfmt::skip]
    let expected: [u8; 83] = [
        0x02, 0x01,
        0x01,
        0xD8, 0x3B, 0xDA, 0x10, 0xD8, 0x1C,
        b'0', b'.', b'9', b'.', b'1', 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0x02,
        0x06,
        0x44, 0x33, 0x22, 0x11,
        0x89, 0x67, 0x05, 0x00,
        // 51: image, 32 bytes in order
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
        0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    ];
    assert_eq!(out[..HELLO_IMAGE_LEN], expected);
    // Nothing written past the image.
    assert_eq!(out[HELLO_IMAGE_LEN], 0xFF);
    assert_eq!(Hello::decode(&expected), Some(hello));
}

#[test]
fn radio_link_hello_decodes_image_none_when_payload_is_51_bytes() {
    // A C6 with no image digest to report sends the 51-byte hello.
    let mut bytes = [0u8; HELLO_LEN];
    bytes[2] = 1;
    assert_eq!(Hello::decode(&bytes).unwrap().image, None);
}

#[test]
fn radio_link_hello_decodes_image_none_when_digest_is_cut_short() {
    let mut bytes = [0u8; HELLO_IMAGE_LEN - 1];
    bytes[2] = 1;
    assert_eq!(Hello::decode(&bytes).unwrap().image, None);
}

#[test]
fn radio_link_hello_decodes_unknown_reset_cause_as_unknown() {
    let mut bytes = [0u8; HELLO_LEN];
    bytes[2] = 1;
    bytes[41] = 0xEE;
    bytes[42] = 0xEE;
    let hello = Hello::decode(&bytes).unwrap();
    assert_eq!(hello.reset_cause, ResetCause::Unknown);
    assert_eq!(hello.last_phase, LoopPhase::Unknown);
}

#[test]
fn radio_link_hello_decodes_unknown_phase_when_code_names_nothing() {
    let mut bytes = [0u8; HELLO_LEN];
    bytes[2] = 1;
    for code in [2, 5, 7, 8, 9] {
        bytes[42] = code;
        assert_eq!(Hello::decode(&bytes).unwrap().last_phase, LoopPhase::Unknown, "code {code}");
    }
    // The frozen code, not the variant's enum index.
    bytes[42] = 10;
    assert_eq!(Hello::decode(&bytes).unwrap().last_phase, LoopPhase::RadioLink);
}

#[test]
fn radio_link_hello_truncates_invalid_utf8_version() {
    let mut bytes = [0u8; HELLO_LEN];
    bytes[2] = 1;
    bytes[9..14].copy_from_slice(&[b'0', b'.', b'9', 0xFF, b'1']);
    let hello = Hello::decode(&bytes).unwrap();
    assert_eq!(hello.fw_version, "0.9");
}

#[test]
fn radio_link_hello_decodes_none_when_chip_unknown() {
    let mut bytes = [0u8; HELLO_LEN];
    bytes[2] = 3;
    assert_eq!(Hello::decode(&bytes), None);
}

#[test]
fn radio_link_hello_decodes_none_when_short() {
    assert_eq!(Hello::decode(&[0u8; HELLO_LEN - 1]), None);
}

#[test]
fn radio_link_packer_refuses_record_that_does_not_fit() {
    let mut record = [0u8; PAYLOAD_MAX];
    let len = encode_down(&largest_send(), &mut record).unwrap();
    let mut payload = [0u8; PAYLOAD_MAX];
    let mut packer = Packer::new(&mut payload);
    assert!(packer.push_encoded(&record[..len]));
    assert_eq!(packer.len(), len + 2);
    assert!(!packer.push_encoded(&record[..len]));
    assert_eq!(packer.len(), len + 2);
}

#[test]
fn radio_link_records_stop_when_len_is_zero() {
    let mut payload = [0u8; 16];
    let mut packer = Packer::new(&mut payload);
    assert!(packer.push_encoded(&[7, 8]));
    // The rest of the payload is zero, which reads as the terminator.
    assert_eq!(records(&payload).collect::<std::vec::Vec<_>>(), [&[7u8, 8][..]]);
}

#[test]
fn radio_link_largest_messages_fit_one_packet() {
    let mut out = [0u8; BODY_MAX];
    let down = encode_down(&largest_send(), &mut out).unwrap();
    assert!(down + 2 <= PAYLOAD_MAX, "largest Down is {down} bytes");
    let rx = Up::Message(BridgeToHost::Rx {
        src: [0x34; 6],
        dst: [0x56; 6],
        rssi: -90,
        rx_us: u32::MAX,
        payload: Vec::from_slice(&[0x5A; MAX_NODE_PAYLOAD]).unwrap(),
    });
    let up = encode_up(&rx, &mut out).unwrap();
    assert!(up + 2 <= PAYLOAD_MAX, "largest Up is {up} bytes");
}

#[test]
fn radio_link_up_is_priority_when_not_a_received_frame() {
    // The C6 queues these apart from the traffic, so a backlog of frames evicts frames.
    let rx = Up::Message(BridgeToHost::Rx {
        src: [1; 6],
        dst: [2; 6],
        rssi: -40,
        rx_us: 0,
        payload: Vec::new(),
    });
    assert!(!rx.is_priority());
    let result =
        Up::Message(BridgeToHost::SendResult { id: 1, status: SendStatus::AckOk, tx_us: 0 });
    assert!(result.is_priority());
    assert!(log("peer added").is_priority());
    let error = Up::Message(BridgeToHost::Error { message: "no".try_into().unwrap() });
    assert!(error.is_priority());
    let status = Up::RadioStatus(RadioStatus {
        peer_count: 0,
        rx_count: 0,
        rx_oversize: 0,
        heap_free: 0,
        uptime_ms: 0,
        queues: Vec::new(),
        crc_fail: 0,
        bad_magic: 0,
        seq_gap: 0,
        uart_errors: 0,
    });
    assert!(status.is_priority());
}

#[test]
fn radio_link_revision_golden_bytes() {
    const MSG: &str =
        "Up/Down encoding changed: bump radio_link::LINK_REVISION and update these bytes";
    let mut out = [0u8; 64];

    let down = Down::Command(HostToBridge::AddPeer { mac: [1, 2, 3, 4, 5, 6] });
    let len = encode_down(&down, &mut out).unwrap();
    assert_eq!(out[..len], [0, 1, 1, 2, 3, 4, 5, 6], "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), down);

    let mut queues = Vec::new();
    queues
        .push(QueueStat { queue: QueueId::RadioUp, capacity: 300, high_water: 2, dropped: 1 })
        .unwrap();
    queues
        .push(QueueStat {
            queue: QueueId::RadioUpPriority,
            capacity: 8192,
            high_water: 3,
            dropped: 0,
        })
        .unwrap();
    let up = Up::RadioStatus(RadioStatus {
        peer_count: 3,
        rx_count: 200,
        rx_oversize: 4,
        heap_free: 5,
        uptime_ms: 6,
        queues,
        crc_fail: 7,
        bad_magic: 8,
        seq_gap: 9,
        uart_errors: 10,
    });
    let len = encode_up(&up, &mut out).unwrap();
    #[rustfmt::skip]
    let expected = [
        1,          // Up::RadioStatus
        3,          // peer_count
        0xC8, 0x01, // rx_count 200, varint
        4, 5, 6,    // rx_oversize, heap_free, uptime_ms
        2,          // queues: two entries
        0,          // QueueId::RadioUp
        0xAC, 0x02, // capacity 300, varint
        2, 1,       // high_water, dropped
        5,          // QueueId::RadioUpPriority
        0x80, 0x40, // capacity 8192, varint
        3, 0,       // high_water, dropped
        7, 8, 9,    // crc_fail, bad_magic, seq_gap
        10,         // uart_errors
    ];
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), up);
}

/// The records that carry the traffic: a change to any of their fields must move
/// `LINK_REVISION` too.
#[test]
fn radio_link_revision_golden_bytes_for_traffic() {
    const MSG: &str =
        "Up/Down encoding changed: bump radio_link::LINK_REVISION and update these bytes";
    let mut out = [0u8; 64];

    let rx = Up::Message(BridgeToHost::Rx {
        src: [1, 2, 3, 4, 5, 6],
        dst: [0xFF; 6],
        rssi: -40,
        rx_us: 1_000,
        payload: Vec::from_slice(&[0xAA, 0xBB, 0xCC]).unwrap(),
    });
    let len = encode_up(&rx, &mut out).unwrap();
    #[rustfmt::skip]
    let expected = [
        0,                                  // Up::Message
        1,                                  // BridgeToHost::Rx
        1, 2, 3, 4, 5, 6,                   // src
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // dst
        0xD8,                               // rssi -40
        0xE8, 0x07,                         // rx_us 1000, varint
        3, 0xAA, 0xBB, 0xCC,                // payload, length first
    ];
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), rx);

    let result =
        Up::Message(BridgeToHost::SendResult { id: 0x1234, status: SendStatus::AckOk, tx_us: 300 });
    let len = encode_up(&result, &mut out).unwrap();
    #[rustfmt::skip]
    let expected = [
        0,          // Up::Message
        2,          // BridgeToHost::SendResult
        0xB4, 0x24, // id 0x1234, varint
        0,          // SendStatus::AckOk
        0xAC, 0x02, // tx_us 300, varint
    ];
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), result);

    let send = Down::Command(HostToBridge::SendEspNow {
        id: 7,
        dst: [9; 6],
        ensure_peer: true,
        payload: Vec::from_slice(&[1, 2]).unwrap(),
    });
    let len = encode_down(&send, &mut out).unwrap();
    #[rustfmt::skip]
    let expected = [
        0,                // Down::Command
        3,                // HostToBridge::SendEspNow
        7,                // id
        9, 9, 9, 9, 9, 9, // dst
        1,                // ensure_peer
        2, 1, 2,          // payload, length first
    ];
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), send);
}

#[test]
fn radio_link_seq_check_reports_gaps_when_seq_skips_or_restarts() {
    let mut check = SeqCheck::new();
    assert_eq!(check.note(10), SeqNote::First);
    assert_eq!(check.note(11), SeqNote::InOrder);
    assert_eq!(check.note(14), SeqNote::Gap);
    // A gap resynchronizes on the seq it saw.
    assert_eq!(check.note(15), SeqNote::InOrder);
    // A restarted sender counts from wherever it starts again.
    assert_eq!(check.note(0), SeqNote::Gap);
    let mut check = SeqCheck::new();
    check.note(u32::MAX);
    assert_eq!(check.note(0), SeqNote::InOrder);
}

#[test]
fn radio_link_seq_check_reads_equal_seq_as_gap() {
    // A sender never sends a packet twice, so a restarted C6 whose only packet was seq 0, and
    // whose next life starts at seq 0, must not read as nothing happened.
    let mut check = SeqCheck::new();
    assert_eq!(check.note(0), SeqNote::First);
    assert_eq!(check.note(0), SeqNote::Gap);
}

#[test]
fn radio_link_held_sends_record_whole_in_next_packet_when_it_does_not_fit() {
    let mut ring = FrameRing::<4096>::new();
    let mut held = Held::new();
    // Two 900-byte records take 1,804 of the payload's bytes; the third needs 302 more.
    for (fill, len) in [(1, 900), (2, 900), (3, 300), (4, 100)] {
        assert!(ring.push(&vec![fill; len]));
    }
    assert_eq!(pack_packet(&mut held, &mut ring), [vec![1; 900], vec![2; 900]]);
    assert_eq!(pack_packet(&mut held, &mut ring), [vec![3; 300], vec![4; 100]]);
}

#[test]
fn radio_link_held_holds_nothing_when_ring_empties_mid_packet() {
    let mut ring = FrameRing::<256>::new();
    let mut held = Held::new();
    assert!(ring.push(&[1; 10]));
    assert!(ring.push(&[2; 20]));
    let mut payload = [0u8; PAYLOAD_MAX];
    let mut packer = Packer::new(&mut payload);
    assert!(packer.push_encoded(&[0; 40]));
    held.pack(&mut packer, |out| ring.read(out).0);
    let len = packer.len();
    assert_eq!(
        records(&payload[..len]).collect::<std::vec::Vec<_>>(),
        [&[0; 40][..], &[1; 10], &[2; 20]]
    );

    // The ring ran dry with room to spare, so nothing is held over.
    assert!(pack_packet(&mut held, &mut ring).is_empty());

    assert!(ring.push(&[3; 5]));
    assert_eq!(pack_packet(&mut held, &mut ring), [vec![3; 5]]);
}

#[test]
fn radio_link_held_fills_packet_exactly_when_record_is_record_max() {
    let mut ring = FrameRing::<4096>::new();
    let mut held = Held::new();
    assert!(ring.push(&[7; RECORD_MAX]));
    let mut payload = [0u8; PAYLOAD_MAX];
    let mut packer = Packer::new(&mut payload);
    held.pack(&mut packer, |out| ring.read(out).0);
    assert_eq!(packer.len(), PAYLOAD_MAX);
    assert_eq!(records(&payload).collect::<std::vec::Vec<_>>(), [&[7; RECORD_MAX][..]]);

    // Behind any other record a `RECORD_MAX` record cannot fit, so it waits for an empty
    // packet, where it always does.
    assert!(ring.push(&[1; 10]));
    assert!(ring.push(&[8; RECORD_MAX]));
    assert_eq!(pack_packet(&mut held, &mut ring), [vec![1; 10]]);
    assert_eq!(pack_packet(&mut held, &mut ring), [vec![8; RECORD_MAX]]);
}
