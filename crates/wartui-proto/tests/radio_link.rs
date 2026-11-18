//! The P4–C6 UART link: its framing, its frozen hello, and the messages it carries.

use heapless::Vec;
use wartui_proto::bonds::{BondAddr, BondLevel, BondRecord, MAX_BONDS};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::link::{LogStr, LoopPhase, QueueId, QueueStat, ShortStr, crc16};
use wartui_proto::radio_link::{
    Accept, BODY_MAX, BleCounters, BleState, CORE_VERSION, CRC_LEN, Chip, Down, HEADER_LEN,
    HELLO_LEN, HOST_BYTES_MAX, HOST_WINDOW_BYTES, Held, Hello, Inflow, Kind, MAGIC, PAYLOAD_MAX,
    Packer, PacketReader, RECORD_MAX, RadioStatus, SeqCheck, SeqNote, Stream, Up, VerifyError,
    WINDOW_UNTIL_BONDED, WIRE_MAX, decode_down, decode_up, encode_down, encode_packet, encode_up,
    host_bytes_fit, host_window, records, seal, verify,
};
use wartui_proto::reset::ResetCause;
use wartui_proto::wardrive_esp::command::COMMAND_MAX;

fn log(text: &str) -> Up<'_> {
    Up::Log { message: text.try_into().unwrap() }
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
        image: digest,
    };
    let mut out = [0xFFu8; 96];
    assert_eq!(hello.encode(&mut out), HELLO_LEN);
    assert_eq!(HELLO_LEN, 79);
    #[rustfmt::skip]
    let expected: [u8; 79] = [
        // 0: link_rev, u16 LE
        0x02, 0x01,
        // 2: chip, Esp32C6 is code 1
        0x01,
        // 3: mac
        0xD8, 0x3B, 0xDA, 0x10, 0xD8, 0x1C,
        // 9: fw_version, "0.9.1" then zeros to 32 bytes
        b'0', b'.', b'9', b'.', b'1', 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        // 41: reset_cause, Watchdog is code 2
        0x02,
        // 42: last_phase, Pump is frozen code 6, whatever its enum index
        0x06,
        // 43: uptime_ms, u32 LE
        0x44, 0x33, 0x22, 0x11,
        // 47: image, 32 bytes in order
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
        0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    ];
    assert_eq!(out[..HELLO_LEN], expected);
    // Nothing written past the image.
    assert_eq!(out[HELLO_LEN], 0xFF);
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
        image: [0; 32],
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
    // A later build may append fields; this build reads the frozen 79 bytes and no more.
    let hello = Hello {
        link_rev: 1,
        chip: Chip::Esp32C6,
        mac: [1, 2, 3, 4, 5, 6],
        fw_version: ShortStr::try_from("0.1.0").unwrap(),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Boot,
        uptime_ms: 72,
        image: [0x5A; 32],
    };
    let mut out = [0xEEu8; HELLO_LEN + 32];
    assert_eq!(hello.encode(&mut out), HELLO_LEN);
    assert_eq!(Hello::decode(&out), Some(hello));
}

#[test]
fn radio_link_hello_decodes_none_when_digest_is_cut_short() {
    let mut bytes = [0u8; HELLO_LEN];
    bytes[2] = 1;
    assert!(Hello::decode(&bytes).is_some());
    assert_eq!(Hello::decode(&bytes[..HELLO_LEN - 1]), None);
    // The layout before the digest, 47 bytes, is not a hello.
    assert_eq!(Hello::decode(&bytes[..47]), None);
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
    for code in [0, 3] {
        bytes[2] = code;
        assert_eq!(Hello::decode(&bytes), None, "code {code}");
    }
}

#[test]
fn radio_link_hello_decodes_none_when_short() {
    assert_eq!(Hello::decode(&[0u8; HELLO_LEN - 1]), None);
}

#[test]
fn radio_link_packer_refuses_record_that_does_not_fit() {
    // A record a little over half a payload: one fits, a second does not.
    let record = [0x5Au8; PAYLOAD_MAX / 2 + 1];
    let len = record.len();
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
    let status = Up::RadioStatus(RadioStatus {
        queues: Vec::from_slice(
            &[QueueStat { queue: QueueId::RadioUp, capacity: u32::MAX, high_water: 0, dropped: 0 };
                4],
        )
        .unwrap(),
        crc_fail: u32::MAX,
        bad_magic: u32::MAX,
        seq_gap: u32::MAX,
        uart_errors: u32::MAX,
        ble: BleCounters {
            pairings: u32::MAX,
            failed_pairings: u32::MAX,
            unbonded_kicks: u32::MAX,
            last_disconnect: u8::MAX,
            resynced_bytes: u32::MAX,
            dropped_writes: u32::MAX,
            phy: u8::MAX,
            interval_us: u32::MAX,
            heap_high_water: u32::MAX,
        },
    });
    let line = Up::Error { message: LogStr::try_from("x".repeat(96).as_str()).unwrap() };
    let stream = [0xFFu8; HOST_BYTES_MAX];
    let state = Up::BleState(BleState {
        stream: Stream::Phone,
        encrypted: true,
        bonded: true,
        mtu: u16::MAX,
    });
    let bond = largest_bond();
    let added = Up::BondAdded(bond);
    let removed = Up::BondRemoved(bond.addr);
    let credit = Up::Credit { accounted: u32::MAX };
    let command = Up::WardriveEspCommand(&stream[..COMMAND_MAX]);
    for up in [status, line, Up::HostBytes(&stream), credit, state, added, removed, command] {
        let len = encode_up(&up, &mut out).unwrap();
        assert!(len <= RECORD_MAX, "{up:?} is {len} bytes");
    }
    let host_bytes = Down::HostBytes { at: u32::MAX, bytes: &stream };
    assert_eq!(encode_down(&host_bytes, &mut out).unwrap(), RECORD_MAX);
    let bonds = Down::Bonds {
        bonds: Vec::from_slice(&[bond; MAX_BONDS]).unwrap(),
        window_ms: WINDOW_UNTIL_BONDED,
        wardrive_esp: true,
    };
    let len = encode_down(&bonds, &mut out).unwrap();
    assert!(len <= RECORD_MAX, "{bonds:?} is {len} bytes");
}

/// A bond with every field at its longest.
fn largest_bond() -> BondRecord {
    BondRecord {
        addr: BondAddr { kind: u8::MAX, addr: [0xFF; 6] },
        irk: Some(u128::MAX),
        ltk: u128::MAX,
        level: BondLevel::Authenticated,
    }
}

#[test]
fn radio_link_host_bytes_fit_room_when_record_is_any_size_and_offset_is_widest() {
    // `at` at u32::MAX takes its varint's widest five bytes.
    let stream = [0x5Au8; RECORD_MAX];
    let mut out = [0u8; BODY_MAX];
    assert_eq!(HOST_BYTES_MAX, RECORD_MAX - 8);
    for room in 0..=RECORD_MAX {
        let fit = host_bytes_fit(room);
        if room < 7 {
            assert_eq!(fit, 0, "room {room}");
            continue;
        }
        let down = Down::HostBytes { at: u32::MAX, bytes: &stream[..fit] };
        let down = encode_down(&down, &mut out).unwrap();
        assert!(down <= room, "room {room}: {fit} bytes took {down}");
        // One more byte would not fit.
        let over = Down::HostBytes { at: u32::MAX, bytes: &stream[..fit + 1] };
        let over = encode_down(&over, &mut out).unwrap();
        assert!(over > room, "room {room}: {} bytes took {over}", fit + 1);
        // No `at`, so the count is conservative for the C6's direction.
        let up = encode_up(&Up::HostBytes(&stream[..fit]), &mut out).unwrap();
        assert_eq!(up + 5, down, "room {room}");
    }
}

#[test]
fn radio_link_packer_pushes_record_written_in_place_when_it_fits() {
    let mut payload = [0u8; 16];
    let mut packer = Packer::new(&mut payload);
    assert!(packer.push_encoded(&[1, 2]));
    // Two records' prefixes and the first's two bytes leave ten.
    assert_eq!(packer.room(), 10);
    let host_bytes = Down::HostBytes { at: 3, bytes: &[7; 7] };
    assert!(packer.push_with(|out| encode_down(&host_bytes, out).ok()));
    assert_eq!(packer.room(), 0);
    assert!(!packer.push_with(|out| encode_down(&Down::GetRadioStatus, out).ok()));
    let len = packer.len();
    assert_eq!(len, 16);
    let got: std::vec::Vec<_> = records(&payload[..len]).collect();
    assert_eq!(got, [&[1u8, 2][..], &[2, 3, 7, 7, 7, 7, 7, 7, 7, 7]]);
}

#[test]
fn radio_link_packer_changes_nothing_when_writer_refuses() {
    let mut payload = [0u8; 16];
    let mut packer = Packer::new(&mut payload);
    assert!(!packer.push_with(|_| None));
    assert!(!packer.push_with(|_| Some(0)));
    assert!(packer.is_empty());
}

#[test]
fn radio_link_window_holds_16_kib_when_built() {
    // Both firmwares read it: the P4 to hold its sends, the C6 to size its notify ring.
    assert_eq!(HOST_WINDOW_BYTES, 16 * 1024);
}

#[test]
fn radio_link_revision_golden_bytes() {
    // LINK_REVISION stays at 1 until merge; a change to these types re-pins the bytes.
    const MSG: &str = "Up/Down encoding changed: re-pin these bytes";
    let mut out = [0u8; 64];
    assert_eq!(wartui_proto::radio_link::LINK_REVISION, 1, "{MSG}");

    let down = Down::GetRadioStatus;
    let len = encode_down(&down, &mut out).unwrap();
    assert_eq!(out[..len], [0], "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), down);

    let mut queues = Vec::new();
    queues
        .push(QueueStat { queue: QueueId::RadioUp, capacity: 300, high_water: 2, dropped: 1 })
        .unwrap();
    let up = Up::RadioStatus(RadioStatus {
        queues,
        crc_fail: 7,
        bad_magic: 8,
        seq_gap: 9,
        uart_errors: 10,
        ble: BleCounters {
            pairings: 1,
            failed_pairings: 2,
            unbonded_kicks: 3,
            last_disconnect: 0x13,
            resynced_bytes: 300,
            dropped_writes: 4,
            phy: 2,
            interval_us: 7_500,
            heap_high_water: 5,
        },
    });
    let len = encode_up(&up, &mut out).unwrap();
    #[rustfmt::skip]
    let expected = [
        0,          // Up::RadioStatus
        1,          // queues: one entry
        0,          // QueueId::RadioUp
        0xAC, 0x02, // capacity 300, varint
        2, 1,       // high_water, dropped
        7, 8, 9,    // crc_fail, bad_magic, seq_gap
        10,         // uart_errors
        1, 2, 3,    // ble: pairings, failed_pairings, unbonded_kicks
        0x13,       // last_disconnect
        0xAC, 0x02, // resynced_bytes 300, varint
        4,          // dropped_writes
        2,          // phy
        0xCC, 0x3A, // interval_us 7500, varint
        5,          // heap_high_water
    ];
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), up);

    let line = Up::Log { message: "hi".try_into().unwrap() };
    let len = encode_up(&line, &mut out).unwrap();
    // Up::Log, then the message, length first.
    assert_eq!(out[..len], [1, 2, b'h', b'i'], "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), line);

    let error = Up::Error { message: "no".try_into().unwrap() };
    let len = encode_up(&error, &mut out).unwrap();
    assert_eq!(out[..len], [2, 2, b'n', b'o'], "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), error);

    let command = Up::WardriveEspCommand(b"caps");
    let len = encode_up(&command, &mut out).unwrap();
    // Up::WardriveEspCommand, then the bytes, length first.
    assert_eq!(out[..len], [8, 4, b'c', b'a', b'p', b's'], "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), command);

    let bonds = Down::Bonds { bonds: Vec::new(), window_ms: 0, wardrive_esp: true };
    let len = encode_down(&bonds, &mut out).unwrap();
    // Down::Bonds, no bonds, window_ms 0, phones admitted.
    assert_eq!(out[..len], [3, 0, 0, 1], "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), bonds);
}

#[test]
fn radio_link_pins_host_stream_bytes_when_encoded() {
    const MSG: &str = "Up/Down encoding changed: re-pin these bytes";
    let mut out = [0u8; 128];

    let start = Down::HostStart { at: 300 };
    let len = encode_down(&start, &mut out).unwrap();
    // Down::HostStart, then `at` as a varint.
    assert_eq!(out[..len], [1, 0xAC, 0x02], "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), start);

    let down = Down::HostBytes { at: u32::MAX, bytes: &[0, 1, 2] };
    let len = encode_down(&down, &mut out).unwrap();
    // Down::HostBytes, `at` at its widest, then the bytes, length first.
    assert_eq!(out[..len], [2, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 3, 0, 1, 2], "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), down);

    let up = Up::HostBytes(&[0xAA, 0xBB]);
    let len = encode_up(&up, &mut out).unwrap();
    // Up::HostBytes, then the bytes, length first.
    assert_eq!(out[..len], [3, 2, 0xAA, 0xBB], "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), up);

    let credit = Up::Credit { accounted: 300 };
    let len = encode_up(&credit, &mut out).unwrap();
    assert_eq!(out[..len], [4, 0xAC, 0x02], "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), credit);

    let state =
        Up::BleState(BleState { stream: Stream::Host, encrypted: true, bonded: false, mtu: 247 });
    let len = encode_up(&state, &mut out).unwrap();
    #[rustfmt::skip]
    let expected = [
        5,          // Up::BleState
        1, 1, 0,    // Stream::Host, encrypted, bonded
        0xF7, 0x01, // mtu 247, varint
    ];
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), state);
}

#[test]
fn radio_link_pins_bond_bytes_when_encoded() {
    const MSG: &str = "Up/Down encoding changed: re-pin these bytes";
    let mut out = [0u8; 128];
    let bond = BondRecord {
        addr: BondAddr { kind: 1, addr: [0x5A, 0x11, 0x22, 0x33, 0x44, 0x02] },
        irk: Some(5),
        ltk: 300,
        level: BondLevel::Authenticated,
    };
    #[rustfmt::skip]
    let bond_bytes = [
        1,                                  // addr.kind
        0x5A, 0x11, 0x22, 0x33, 0x44, 0x02, // addr.addr
        1, 5,                               // irk: Some, 5 as a varint
        0xAC, 0x02,                         // ltk 300, varint
        1,                                  // BondLevel::Authenticated
    ];

    let down = Down::Bonds {
        bonds: Vec::from_slice(&[bond]).unwrap(),
        window_ms: 120_000,
        wardrive_esp: false,
    };
    let len = encode_down(&down, &mut out).unwrap();
    let mut expected = vec![3, 1]; // Down::Bonds, one bond
    expected.extend_from_slice(&bond_bytes);
    expected.extend_from_slice(&[0xC0, 0xA9, 0x07]); // window_ms 120000, varint
    expected.push(0); // phones not admitted
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), down);

    let until =
        Down::Bonds { bonds: Vec::new(), window_ms: WINDOW_UNTIL_BONDED, wardrive_esp: false };
    let len = encode_down(&until, &mut out).unwrap();
    assert_eq!(out[..len], [3, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0], "{MSG}");
    assert_eq!(decode_down(&out[..len]).unwrap(), until);

    let added = Up::BondAdded(bond);
    let len = encode_up(&added, &mut out).unwrap();
    let mut expected = vec![6]; // Up::BondAdded
    expected.extend_from_slice(&bond_bytes);
    assert_eq!(out[..len], expected, "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), added);

    let removed = Up::BondRemoved(bond.addr);
    let len = encode_up(&removed, &mut out).unwrap();
    assert_eq!(out[..len], [7, 1, 0x5A, 0x11, 0x22, 0x33, 0x44, 0x02], "{MSG}");
    assert_eq!(decode_up(&out[..len]).unwrap(), removed);
}

#[test]
fn radio_link_round_trips_wardrive_command_when_encoded() {
    let mut out = [0u8; RECORD_MAX];
    let longest = [b'x'; COMMAND_MAX];
    for text in [&b"wdstream start -wifi -ch 1,6,11"[..], &[0xFF, 0x00, b'\n'], &longest] {
        let command = Up::WardriveEspCommand(text);
        let len = encode_up(&command, &mut out).unwrap();
        assert_eq!(decode_up(&out[..len]).unwrap(), command);
    }
}

#[test]
fn radio_link_encodes_each_stream_as_one_byte_when_state_is_built() {
    let mut out = [0u8; 16];
    for (stream, byte) in [(Stream::None, 0), (Stream::Host, 1), (Stream::Phone, 2)] {
        let state = Up::BleState(BleState { stream, encrypted: false, bonded: false, mtu: 23 });
        let len = encode_up(&state, &mut out).unwrap();
        // Up::BleState, the stream, encrypted, bonded, mtu.
        assert_eq!(out[..len], [5, byte, 0, 0, 23], "{stream:?}");
        assert_eq!(decode_up(&out[..len]).unwrap(), state);
    }
    assert_eq!(BleState::default().stream, Stream::None);
}

#[test]
fn radio_link_round_trips_bonds_when_phones_are_admitted() {
    let mut out = [0u8; RECORD_MAX];
    let bonds = Down::Bonds {
        bonds: Vec::from_slice(&[largest_bond(); MAX_BONDS]).unwrap(),
        window_ms: 120_000,
        wardrive_esp: true,
    };
    let len = encode_down(&bonds, &mut out).unwrap();
    // The flag is the record's last byte.
    assert_eq!(out[len - 1], 1);
    assert_eq!(decode_down(&out[..len]).unwrap(), bonds);
}

#[test]
fn inflow_takes_everything_when_bytes_arrive_in_order() {
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(1_000), 0);
    assert_eq!(inflow.accept(1_000, 100), Accept::Take { skip: 0, gap: 0 });
    assert_eq!(inflow.accept(1_100, 50), Accept::Take { skip: 0, gap: 0 });
}

#[test]
fn inflow_adopts_first_offset_when_nothing_was_expected() {
    // A C6 fresh from boot has no base, and adopts the P4's offsets from the first it sees.
    let mut inflow = Inflow::new();
    assert_eq!(inflow.accept(5_000, 10), Accept::Take { skip: 0, gap: 0 });
    assert_eq!(inflow.accept(5_010, 10), Accept::Take { skip: 0, gap: 0 });
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(7_000), 0);
}

#[test]
fn inflow_reports_gap_when_a_packet_was_lost() {
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(0), 0);
    assert_eq!(inflow.accept(0, 100), Accept::Take { skip: 0, gap: 0 });
    // The packet carrying 100..300 failed its CRC.
    assert_eq!(inflow.accept(300, 50), Accept::Take { skip: 0, gap: 200 });
    assert_eq!(inflow.accept(350, 50), Accept::Take { skip: 0, gap: 0 });
}

#[test]
fn inflow_takes_nothing_when_bytes_are_a_full_duplicate() {
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(0), 0);
    assert_eq!(inflow.accept(0, 100), Accept::Take { skip: 0, gap: 0 });
    assert_eq!(inflow.accept(40, 60), Accept::Duplicate);
    assert_eq!(inflow.accept(0, 0), Accept::Duplicate);
    // Nothing moved: the next in order is still at 100.
    assert_eq!(inflow.accept(100, 10), Accept::Take { skip: 0, gap: 0 });
}

#[test]
fn inflow_skips_taken_bytes_when_bytes_partly_overlap() {
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(0), 0);
    assert_eq!(inflow.accept(0, 100), Accept::Take { skip: 0, gap: 0 });
    assert_eq!(inflow.accept(80, 50), Accept::Take { skip: 20, gap: 0 });
    assert_eq!(inflow.accept(130, 10), Accept::Take { skip: 0, gap: 0 });
}

#[test]
fn inflow_reports_gap_when_loss_spans_the_u32_wrap() {
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(u32::MAX - 99), 0);
    assert_eq!(inflow.accept(u32::MAX - 99, 50), Accept::Take { skip: 0, gap: 0 });
    // Expected at u32::MAX - 49; these start 60 past the wrap.
    assert_eq!(inflow.accept(60, 10), Accept::Take { skip: 0, gap: 110 });
    assert_eq!(inflow.accept(70, 10), Accept::Take { skip: 0, gap: 0 });
    // Behind across the wrap reads as behind.
    assert_eq!(inflow.accept(u32::MAX - 9, 10), Accept::Duplicate);
}

#[test]
fn inflow_discards_late_bytes_when_start_begins_a_new_attachment() {
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(0), 0);
    assert_eq!(inflow.accept(0, 100), Accept::Take { skip: 0, gap: 0 });
    assert_eq!(inflow.start(100), 0);
    // An earlier attachment's bytes are behind the new one's start.
    assert_eq!(inflow.accept(90, 10), Accept::Duplicate);
    assert_eq!(inflow.accept(95, 15), Accept::Take { skip: 5, gap: 0 });
}

#[test]
fn inflow_counts_lost_bytes_when_start_jumps_past_expected() {
    // The UART is ordered, so bytes before the start that have not arrived never will.
    let mut inflow = Inflow::new();
    assert_eq!(inflow.start(0), 0);
    assert_eq!(inflow.accept(0, 100), Accept::Take { skip: 0, gap: 0 });
    // The earlier attachment's last packet, 100..180, was lost.
    assert_eq!(inflow.start(180), 80);
    assert_eq!(inflow.accept(180, 20), Accept::Take { skip: 0, gap: 0 });
}

#[test]
fn inflow_expects_nothing_when_new() {
    assert_eq!(Inflow::new().expected(), None);
}

#[test]
fn inflow_expects_start_offset_when_started() {
    let mut inflow = Inflow::new();
    let _ = inflow.start(7_000);
    assert_eq!(inflow.expected(), Some(7_000));
    // A start behind the expected offset moves it back: the P4 names where the attachment begins.
    let _ = inflow.start(6_000);
    assert_eq!(inflow.expected(), Some(6_000));
}

#[test]
fn inflow_expects_end_of_bytes_when_bytes_arrive_in_order() {
    let mut inflow = Inflow::new();
    let _ = inflow.accept(5_000, 10);
    assert_eq!(inflow.expected(), Some(5_010), "the first offset seen is the base");
    let _ = inflow.accept(5_010, 30);
    assert_eq!(inflow.expected(), Some(5_040));
}

#[test]
fn inflow_expects_past_the_gap_when_a_packet_was_lost() {
    // The credit is read from the position, so it covers the lost bytes with nothing added.
    let mut inflow = Inflow::new();
    let _ = inflow.start(0);
    let _ = inflow.accept(0, 100);
    assert_eq!(inflow.accept(300, 50), Accept::Take { skip: 0, gap: 200 });
    assert_eq!(inflow.expected(), Some(350));
}

#[test]
fn inflow_keeps_expected_when_bytes_are_a_duplicate() {
    let mut inflow = Inflow::new();
    let _ = inflow.start(0);
    let _ = inflow.accept(0, 100);
    assert_eq!(inflow.accept(40, 60), Accept::Duplicate);
    assert_eq!(inflow.expected(), Some(100));
}

#[test]
fn host_window_opens_fully_when_nothing_is_in_flight() {
    assert_eq!(host_window(0, 0), HOST_WINDOW_BYTES);
    assert_eq!(host_window(123_456, 123_456), HOST_WINDOW_BYTES);
}

#[test]
fn host_window_shrinks_by_bytes_in_flight_when_part_is_uncredited() {
    assert_eq!(host_window(11_000, 10_000), HOST_WINDOW_BYTES - 1_000);
}

#[test]
fn host_window_closes_when_a_full_window_is_in_flight() {
    assert_eq!(host_window(10_000 + HOST_WINDOW_BYTES, 10_000), 0);
}

#[test]
fn host_window_counts_in_flight_when_offsets_wrap_past_u32() {
    assert_eq!(host_window(400, u32::MAX - 99), HOST_WINDOW_BYTES - 500);
}

#[test]
fn host_window_opens_fully_when_counters_disagree() {
    // A credit ahead of `sent`, or more than a window behind it, cannot happen while the two
    // agree. A closed window would never reopen; an open one lets the next send resynchronize.
    assert_eq!(host_window(10_000, 10_001), HOST_WINDOW_BYTES);
    assert_eq!(host_window(10_000 + HOST_WINDOW_BYTES + 1, 10_000), HOST_WINDOW_BYTES);
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
    assert!(held.is_empty());
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
    assert!(!held.is_empty(), "the record that did not fit is held");
    assert_eq!(pack_packet(&mut held, &mut ring), [vec![8; RECORD_MAX]]);
}
