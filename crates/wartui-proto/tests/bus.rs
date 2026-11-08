//! The node bus's transfer framing, the records it carries, its acknowledgment and its slots.
//!
//! The vectors are written out by hand from the layout table in `bus/mod.rs`. Each CRC was
//! worked out with a separate bitwise CRC-16/CCITT-FALSE, checked against the standard
//! "123456789" value 0x29B1.

use wartui_proto::bus::{
    ACK_TIMEOUT_MS, ACKS, AckPoll, AckTracker, AckWait, BUS_VERSION, CRC_OFFSET, DownType,
    Downlink, FLIGHT, FLIGHT_MAX, FLIGHT_NUMBERS, FlightTag, HEADER_LEN, Header, MAGIC, MAX_SLOTS,
    NodeBoard, PAYLOAD_MAX, Packer, RECORD_HEADER_LEN, RECORD_MAX, SEND_TRIES, SYNC, SeqCheck,
    SeqNote, Slot, TRANSFER_LEN, UpType, VerifyError, WINDOW, records, seal, verify,
};
use wartui_proto::link::{MAX_NODE_DOWN, MAX_NODE_PAYLOAD, Undelivered, crc16};

/// An idle transfer: no records.
#[rustfmt::skip]
const IDLE_HEADER: [u8; HEADER_LEN] = [
    0x57, 0x42, // magic "WB"
    0x01, // bus version 1
    0x01, // flags: ACKS
    0x02, 0x01, // seq 0x0102, little-endian
    0x04, 0x03, // ack 0x0304, little-endian
    0x00, 0x00, // len 0
];
/// CRC-16/CCITT-FALSE 0x3099 over the header and 1,032 zero bytes, little-endian.
const IDLE_CRC: [u8; 2] = [0x99, 0x30];

/// A transfer carrying one record.
#[rustfmt::skip]
const ONE_RECORD: [u8; HEADER_LEN + 7] = [
    0x57, 0x42, // magic "WB"
    0x01, // bus version 1
    0x01, // flags: ACKS
    0x07, 0x00, // seq 7
    0x05, 0x00, // ack 5
    0x07, 0x00, // len 7: one record
    0x01, // record type 1
    0x04, 0x00, // record len 4
    0xDE, 0xAD, 0xBE, 0xEF, // the body, opaque to this layer
];
/// CRC-16/CCITT-FALSE 0x5E4E over those bytes and the zero padding, little-endian.
const ONE_RECORD_CRC: [u8; 2] = [0x4E, 0x5E];

/// The same record in flight 2, with SYNC: only the flags byte differs.
#[rustfmt::skip]
const FLIGHT_RECORD: [u8; HEADER_LEN + 7] = [
    0x57, 0x42, // magic "WB"
    0x01, // bus version 1
    0x0D, // flags: ACKS, flight 2 in bits 1-2, SYNC in bit 3
    0x07, 0x00, // seq 7
    0x05, 0x00, // ack 5
    0x07, 0x00, // len 7: one record
    0x01, // record type 1
    0x04, 0x00, // record len 4
    0xDE, 0xAD, 0xBE, 0xEF, // the body
];
/// CRC-16/CCITT-FALSE 0xECA6 over those bytes and the zero padding, little-endian.
const FLIGHT_RECORD_CRC: [u8; 2] = [0xA6, 0xEC];

fn idle(seq: u16) -> [u8; TRANSFER_LEN] {
    let mut buf = [0u8; TRANSFER_LEN];
    seal(&mut buf, Header { flags: 0, seq, ack: 0 }, 0);
    buf
}

fn rewrite_crc(buf: &mut [u8; TRANSFER_LEN]) {
    let crc = crc16(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
}

/// These sizes are the bus's frozen core. They must never change: any host and P4 reach any node
/// build through them.
#[test]
fn bus_has_pinned_sizes_when_built() {
    assert_eq!(
        (MAGIC, BUS_VERSION, TRANSFER_LEN, HEADER_LEN, PAYLOAD_MAX, RECORD_MAX),
        (0x4257, 1, 1044, 10, 1032, 1029)
    );
    assert_eq!((CRC_OFFSET, ACKS, RECORD_HEADER_LEN), (1042, 0x01, 3));
    assert_eq!((FLIGHT, SYNC, FLIGHT_NUMBERS, WINDOW), (0x06, 0x08, 4, 2));
    assert_eq!(TRANSFER_LEN % 4, 0);
}

/// These bytes are the frozen transfer header and CRC. They must never change.
#[test]
fn bus_seals_pinned_bytes_when_idle() {
    // Junk where the payload goes: `seal` zeroes what nothing wrote.
    let mut buf = [0xAAu8; TRANSFER_LEN];
    seal(&mut buf, Header::new(0x0102, Some(0x0304)), 0);
    assert_eq!(buf[..HEADER_LEN], IDLE_HEADER);
    assert!(buf[HEADER_LEN..CRC_OFFSET].iter().all(|&b| b == 0));
    assert_eq!(buf[CRC_OFFSET..], IDLE_CRC);
}

/// These bytes are the frozen transfer and record layout. They must never change.
#[test]
fn bus_seals_pinned_bytes_when_record_packed() {
    let mut buf = [0xAAu8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(1, &[0xDE, 0xAD, 0xBE, 0xEF]));
    packer.seal(Header::new(7, Some(5)));
    assert_eq!(buf[..ONE_RECORD.len()], ONE_RECORD);
    assert!(buf[ONE_RECORD.len()..CRC_OFFSET].iter().all(|&b| b == 0));
    assert_eq!(buf[CRC_OFFSET..], ONE_RECORD_CRC);
}

/// These bytes are the frozen flight tag. They must never change.
#[test]
fn bus_seals_pinned_bytes_when_flight_tagged() {
    let mut buf = [0xAAu8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(1, &[0xDE, 0xAD, 0xBE, 0xEF]));
    let tag = FlightTag { number: 2, sync: true };
    packer.seal(Header::new(7, Some(5)).with_flight(tag));
    assert_eq!(buf[..FLIGHT_RECORD.len()], FLIGHT_RECORD);
    assert_eq!(buf[CRC_OFFSET..], FLIGHT_RECORD_CRC);
    let header = verify(&buf).unwrap().header;
    assert_eq!((header.flight(), header.acked()), (tag, Some(5)));
}

#[test]
fn bus_round_trips_records_when_several_packed() {
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(1, b"first"));
    assert!(packer.push(2, b""));
    assert!(packer.push(3, b"third"));
    let len = packer.len();
    packer.seal(Header { flags: ACKS, seq: 0xFFFF, ack: 0x1234 });

    let transfer = verify(&buf).unwrap();
    assert_eq!(transfer.header, Header { flags: ACKS, seq: 0xFFFF, ack: 0x1234 });
    assert_eq!(transfer.header.acked(), Some(0x1234));
    assert!(transfer.carries_records());
    assert_eq!(transfer.payload.len(), len);
    let all: Vec<(u8, &[u8])> = transfer.records().map(|r| (r.kind, r.body)).collect();
    assert_eq!(all, [(1, &b"first"[..]), (2, b""), (3, b"third")]);
}

#[test]
fn bus_round_trips_empty_payload_when_idle() {
    let buf = idle(3);
    let transfer = verify(&buf).unwrap();
    assert_eq!(transfer.header, Header { flags: 0, seq: 3, ack: 0 });
    assert!(!transfer.carries_records());
    assert_eq!(transfer.records().count(), 0);
}

#[test]
fn bus_carries_longest_body_when_transfer_empty() {
    const { assert!(RECORD_MAX >= MAX_NODE_PAYLOAD && RECORD_MAX == MAX_NODE_DOWN) };
    let frame = [0x5Au8; MAX_NODE_DOWN];
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(DownType::UpdateChunk.as_u8(), &frame));
    packer.seal(Header::default());
    let transfer = verify(&buf).unwrap();
    let mut records = transfer.records();
    assert_eq!(records.next().unwrap().body, frame);
    assert_eq!(records.next(), None);
}

#[test]
fn bus_refuses_record_when_it_does_not_fit() {
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(1, &[1; 1000]));
    let len = packer.len();
    // One byte of body past what is left: refused, not truncated, and nothing written.
    let left = PAYLOAD_MAX - len - RECORD_HEADER_LEN;
    assert!(!packer.fits(left + 1));
    assert!(!packer.push(1, &vec![2; left + 1]));
    assert_eq!(packer.len(), len);
    assert!(packer.push(1, &vec![3; left]));
    assert_eq!(packer.len(), PAYLOAD_MAX);
}

#[test]
fn bus_refuses_record_when_longer_than_record_max() {
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(!packer.push(1, &[0; RECORD_MAX + 1]));
    assert!(packer.is_empty());
}

#[test]
fn bus_rejects_bad_magic_when_magic_flipped() {
    let mut buf = idle(7);
    buf[0] ^= 0x01;
    assert_eq!(verify(&buf).unwrap_err(), VerifyError::BadMagic);
}

#[test]
fn bus_rejects_bad_magic_when_slot_reads_constant_bytes() {
    // An empty slot behind a pulled-up MISO.
    assert_eq!(verify(&[0xFF; TRANSFER_LEN]).unwrap_err(), VerifyError::BadMagic);
}

#[test]
fn bus_rejects_bad_crc_when_payload_byte_flipped() {
    let mut buf = idle(7);
    buf[500] ^= 0x01;
    assert_eq!(verify(&buf).unwrap_err(), VerifyError::BadCrc);
}

#[test]
fn bus_rejects_bad_crc_when_version_corrupted() {
    // The CRC is checked first, so a flipped version byte reads as corruption.
    let mut buf = idle(7);
    buf[2] = 2;
    assert_eq!(verify(&buf).unwrap_err(), VerifyError::BadCrc);
}

#[test]
fn bus_rejects_other_version_when_version_changed_under_good_crc() {
    let mut buf = idle(7);
    buf[2] = 2;
    rewrite_crc(&mut buf);
    assert_eq!(verify(&buf).unwrap_err(), VerifyError::OtherVersion(2));
}

#[test]
fn bus_rejects_bad_len_when_len_past_payload_max() {
    let mut buf = idle(7);
    let len = u16::try_from(PAYLOAD_MAX + 1).unwrap();
    buf[8..10].copy_from_slice(&len.to_le_bytes());
    rewrite_crc(&mut buf);
    assert_eq!(verify(&buf).unwrap_err(), VerifyError::BadLen);
}

#[test]
fn bus_ends_records_when_record_runs_past_payload() {
    // A good record, then a header claiming more than is left.
    let payload = [1, 2, 0, 0xAA, 0xBB, 1, 9, 0, 0xCC];
    let bodies: Vec<&[u8]> = records(&payload).map(|r| r.body).collect();
    assert_eq!(bodies, [&[0xAA, 0xBB][..]]);
}

#[test]
fn bus_ends_records_when_record_header_cut_short() {
    let payload = [1, 1, 0, 0xAA, 1, 0];
    assert_eq!(records(&payload).count(), 1);
}

/// Skipping an unknown type is part of the bus's frozen core: a later build's new message must
/// never cost a receiver the records beside it.
#[test]
fn bus_yields_record_when_type_unknown() {
    // Skipping a record is the receiver's choice, so the iterator hands it over.
    let payload = [9, 1, 0, 0xAA, 1, 1, 0, 0xBB];
    let all: Vec<(u8, Option<UpType>)> =
        records(&payload).map(|r| (r.kind, UpType::from_u8(r.kind))).collect();
    assert_eq!(all, [(9, None), (1, Some(UpType::Heartbeat))]);
}

#[test]
fn bus_notes_first_when_nothing_seen() {
    let mut check = SeqCheck::new();
    assert_eq!(check.note(40), SeqNote::First);
    assert_eq!(check.note(41), SeqNote::InOrder);
}

#[test]
fn bus_notes_in_order_when_seq_wraps() {
    let mut check = SeqCheck::new();
    check.note(0xFFFF);
    assert_eq!(check.note(0), SeqNote::InOrder);
    assert_eq!(check.note(1), SeqNote::InOrder);
}

#[test]
fn bus_notes_gap_when_seq_skips_or_restarts() {
    let mut check = SeqCheck::new();
    check.note(10);
    assert_eq!(check.note(12), SeqNote::Gap);
    assert_eq!(check.note(13), SeqNote::InOrder);
    // A sender that restarted begins at 0 again.
    assert_eq!(check.note(0), SeqNote::Gap);
    assert_eq!(check.note(0), SeqNote::Gap);
}

/// The `ACKS` flag is part of the bus's frozen core. Its meaning must never change.
#[test]
fn bus_header_acks_nothing_when_acks_flag_clear() {
    // An ack of 0 is a real seq only with the flag beside it.
    assert_eq!(Header::new(1, None).acked(), None);
    assert_eq!(Header { flags: 0, seq: 1, ack: 0 }.acked(), None);
    let header = Header::new(1, Some(0));
    assert_eq!(header.flags, ACKS);
    assert_eq!(header.acked(), Some(0));
}

/// The ack rule is part of the bus's frozen core: idle transfers must never move the ack.
#[test]
fn ack_tracker_keeps_last_record_seq_when_idle_transfers_follow() {
    let mut tracker = AckTracker::new();
    assert_eq!(tracker.ack(), None);
    tracker.note_records(9);
    // Idle transfers are never noted, so the ack stays on the data transfer.
    assert_eq!(tracker.ack(), Some(9));
    tracker.note_records(12);
    assert_eq!(tracker.ack(), Some(12));
}

#[test]
fn ack_wait_ends_when_peer_acks_the_transfer_waited_on() {
    let mut wait = AckWait::new();
    assert_eq!(wait.poll(0), AckPoll::Idle);
    wait.sent(4, 100);
    assert!(!wait.on_header(&Header::new(0, None), 5));
    assert!(!wait.on_header(&Header::new(0, Some(3)), 6), "an older transfer's ack");
    assert_eq!(wait.poll(100 + ACK_TIMEOUT_MS - 1), AckPoll::Waiting);
    assert!(wait.on_header(&Header::new(0, Some(4)), 7));
    assert!(!wait.is_waiting());
}

#[test]
fn ack_wait_ignores_ack_when_it_came_in_the_transfer_it_names() {
    let mut wait = AckWait::new();
    wait.sent(4, 0);
    // The node sealed that header before it saw transfer 4, so its ack of 4 is stale.
    assert!(!wait.on_header(&Header::new(0, Some(4)), 4));
    assert!(wait.is_waiting());
    assert!(wait.on_header(&Header::new(0, Some(4)), 5));
}

#[test]
fn ack_wait_resends_then_gives_up_when_no_ack_comes() {
    let mut wait = AckWait::new();
    let mut now = 0;
    wait.sent(1, now);
    for try_ in 2..=SEND_TRIES {
        now += ACK_TIMEOUT_MS;
        assert_eq!(wait.poll(now), AckPoll::Resend, "try {try_}");
        wait.sent(u16::from(try_), now);
    }
    now += ACK_TIMEOUT_MS;
    assert_eq!(wait.poll(now), AckPoll::GaveUp);
    assert_eq!(wait.poll(now), AckPoll::Idle);
}

#[test]
fn ack_wait_takes_resend_ack_when_only_the_new_transfer_is_named() {
    let mut wait = AckWait::new();
    wait.sent(1, 0);
    assert_eq!(wait.poll(ACK_TIMEOUT_MS), AckPoll::Resend);
    wait.sent(5, ACK_TIMEOUT_MS);
    // The resend went out in transfer 5; the first transfer's seq no longer counts.
    assert!(!wait.on_header(&Header::new(0, Some(1)), 6));
    assert!(wait.on_header(&Header::new(0, Some(5)), 6));
}

#[test]
fn slot_formats_bus_letter_and_index_when_displayed() {
    assert_eq!(Slot::new(0, 0).unwrap().to_string(), "A1");
    assert_eq!(Slot::new(0, 6).unwrap().to_string(), "A7");
    assert_eq!(Slot::new(1, 0).unwrap().to_string(), "B1");
    assert_eq!(Slot::new(1, 6).unwrap().to_string(), "B7");
    assert_eq!(Slot::new(1, 0).unwrap().as_u8(), 7);
}

#[test]
fn slot_parses_text_form_when_valid() {
    for byte in 0..u8::try_from(MAX_SLOTS).unwrap() {
        let slot = Slot::from_u8(byte).unwrap();
        assert_eq!(Slot::parse(&slot.to_string()), Some(slot));
    }
    assert_eq!(Slot::parse("b3"), Slot::new(1, 2));
    for bad in ["", "A", "A0", "A8", "C1", "A10", "1A"] {
        assert_eq!(Slot::parse(bad), None, "{bad}");
    }
}

#[test]
fn slot_names_its_byte_bit_when_read_as_presence_mask() {
    assert_eq!(Slot::new(0, 0).expect("A1").bit(), 1);
    // B3 is byte 9.
    assert_eq!(Slot::new(1, 2).expect("B3").bit(), 1 << 9);
}

#[test]
fn slot_refuses_byte_when_past_max_slots() {
    let max = u8::try_from(MAX_SLOTS).unwrap();
    assert!(Slot::from_u8(max - 1).is_some());
    assert!(Slot::from_u8(max).is_none());
    assert!(Slot::new(2, 0).is_none());
    assert!(Slot::new(0, 7).is_none());
}

fn label(board: NodeBoard, bus: u8, index: u8) -> String {
    board.label(Slot::new(bus, index).unwrap()).to_string()
}

#[test]
fn node_board_labels_b1_by_silk_when_board_is_a_carrier() {
    assert_eq!(label(NodeBoard::Bench, 1, 0), "B1");
    assert_eq!(label(NodeBoard::Carrier6, 1, 0), "N4");
    assert_eq!(label(NodeBoard::Carrier10, 1, 0), "N6");
    assert_eq!(label(NodeBoard::Carrier14, 1, 0), "N8");
}

#[test]
fn node_board_labels_last_slot_when_board_drives_it() {
    assert_eq!(label(NodeBoard::Bench, 1, 5), "B6");
    assert_eq!(label(NodeBoard::Carrier6, 1, 2), "N6");
    assert_eq!(label(NodeBoard::Carrier10, 1, 4), "N10");
    assert_eq!(label(NodeBoard::Carrier14, 1, 6), "N14");
    assert_eq!(label(NodeBoard::Carrier14, 0, 6), "N7");
}

#[test]
fn node_board_keeps_bus_name_when_slot_is_not_driven() {
    assert_eq!(label(NodeBoard::Carrier6, 0, 3), "A4");
    assert_eq!(label(NodeBoard::Carrier10, 1, 6), "B7");
    assert_eq!(label(NodeBoard::Bench, 0, 6), "A7");
}

#[test]
fn node_board_lists_driven_slots_bus_a_first_when_asked() {
    for board in NodeBoard::ALL {
        let slots: Vec<Slot> = board.slots().collect();
        assert_eq!(slots.len(), usize::from(board.slot_count()), "{board}");
        assert!(slots.iter().all(|&slot| board.drives(slot)), "{board}");
        assert!(slots.windows(2).all(|w| w[0] < w[1]), "{board}");
        let labels: Vec<String> = slots.iter().map(|&s| board.label(s).to_string()).collect();
        if board.is_carrier() {
            let silk: Vec<String> = (1..=board.slot_count()).map(|n| format!("N{n}")).collect();
            assert_eq!(labels, silk, "{board}");
        }
    }
    assert_eq!(NodeBoard::Bench.slots().last(), Slot::new(1, 5));
}

#[test]
fn node_board_parses_every_label_it_gives_when_round_tripped() {
    for board in NodeBoard::ALL {
        for slot in board.slots() {
            let text = board.label(slot).to_string();
            assert_eq!(board.parse_label(&text), Some(slot), "{board} {text}");
            assert_eq!(board.parse_label(&text.to_lowercase()), Some(slot), "{board} {text}");
        }
    }
}

#[test]
fn node_board_refuses_label_when_board_does_not_take_it() {
    let refused = [
        (NodeBoard::Bench, ["A7", "B7", "N1", "A0", "", "C1"].as_slice()),
        (NodeBoard::Carrier6, ["N0", "N7", "A1", "B1", "N", "N01", "N+1", "N1x", "1"].as_slice()),
        (NodeBoard::Carrier10, ["N11", "N256", "A1"].as_slice()),
        (NodeBoard::Carrier14, ["N15", "B7", "NN1", " N1"].as_slice()),
    ];
    for (board, names) in refused {
        for name in names {
            assert_eq!(board.parse_label(name), None, "{board} {name:?}");
        }
    }
}

#[test]
fn node_board_names_board_when_displayed() {
    let names: Vec<&str> = NodeBoard::ALL.iter().map(|b| b.name()).collect();
    assert_eq!(names, ["bench", "carrier-6", "carrier-10", "carrier-14"]);
    assert_eq!(NodeBoard::Carrier10.to_string(), "carrier-10");
}

#[test]
fn node_board_pins_link_byte_when_encoded() {
    let mut buf = [0u8; 4];
    for (code, board) in (0u8..).zip(NodeBoard::ALL) {
        assert_eq!(postcard::to_slice(&board, &mut buf).unwrap(), [code], "{board}");
        assert_eq!(postcard::from_bytes::<NodeBoard>(&[code]).unwrap(), board);
    }
    assert!(postcard::from_bytes::<NodeBoard>(&[4]).is_err());
}

const FLIGHT_BYTES: usize = 4096;

/// What a node makes of a transfer the downlink filled.
fn sent_records(buf: &[u8; TRANSFER_LEN], len: usize) -> Vec<(u8, Vec<u8>)> {
    records(&buf[HEADER_LEN..HEADER_LEN + len]).map(|r| (r.kind, r.body.to_vec())).collect()
}

#[test]
fn downlink_packs_queued_records_into_one_flight_when_they_fit() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    down.push(1, 1, &[0xAA; 8]).unwrap();
    down.push(2, 2, &[]).unwrap();
    assert!(down.has_work());
    let mut buf = [0u8; TRANSFER_LEN];
    let len = down.fill(&mut buf, Header::default());
    assert_eq!(sent_records(&buf, len), [(1, vec![0xAA; 8]), (2, vec![])]);
    down.sent(10, 0);
    assert!(!down.has_work(), "everything queued is out");
    assert_eq!(down.fill(&mut buf, Header::default()), 0);
    assert_eq!(down.on_header(&Header::new(0, Some(9)), 11), None);
    assert_eq!(down.on_header(&Header::new(0, Some(10)), 11).unwrap(), [1, 2]);
    assert!(!down.has_work());
}

#[test]
fn downlink_leaves_record_for_next_flight_when_it_does_not_fit() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    down.push(1, 4, &[1; MAX_NODE_DOWN]).unwrap();
    down.push(2, 4, &[2; MAX_NODE_DOWN]).unwrap();
    let mut buf = [0u8; TRANSFER_LEN];
    let len = down.fill(&mut buf, Header::default());
    assert_eq!(sent_records(&buf, len).len(), 1);
    down.sent(1, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(1)), 2).unwrap(), [1]);
    assert!(down.has_work());
    let len = down.fill(&mut buf, Header::default());
    assert_eq!(sent_records(&buf, len), [(4, vec![2; MAX_NODE_DOWN])]);
}

#[test]
fn downlink_caps_flight_when_records_exceed_flight_max() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    for id in 0..=u16::try_from(FLIGHT_MAX).unwrap() {
        down.push(id, 2, &[]).unwrap();
    }
    let mut buf = [0u8; TRANSFER_LEN];
    let len = down.fill(&mut buf, Header::default());
    assert_eq!(sent_records(&buf, len).len(), FLIGHT_MAX);
}

#[test]
fn downlink_resends_same_flight_then_gives_up_when_unacked() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    down.push(7, 1, b"assign").unwrap();
    let mut buf = [0u8; TRANSFER_LEN];
    let first = down.fill(&mut buf, Header::default());
    let sent = sent_records(&buf, first);
    let mut now = 0;
    down.sent(1, now);
    for seq in 2..=u16::from(SEND_TRIES) {
        now += ACK_TIMEOUT_MS;
        assert_eq!(down.poll(now), None);
        assert!(down.has_work());
        let len = down.fill(&mut buf, Header::default());
        assert_eq!(sent_records(&buf, len), sent, "a resend is the same records");
        down.sent(seq, now);
    }
    now += ACK_TIMEOUT_MS;
    assert_eq!(down.poll(now).unwrap(), [7]);
    assert!(!down.has_work());
}

#[test]
fn downlink_ignores_ack_when_it_came_in_the_transfer_it_names() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    down.push(3, 1, b"assign").unwrap();
    let mut buf = [0u8; TRANSFER_LEN];
    down.fill(&mut buf, Header::default());
    down.sent(8, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(8)), 8), None, "a restarted node's ack");
    assert!(down.deadline_ms().is_some(), "still waits for its ack");
    assert_eq!(down.on_header(&Header::new(0, Some(8)), 9).unwrap(), [3]);
}

#[test]
fn downlink_carries_same_flight_when_not_marked_sent() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    down.push(5, 1, b"assign").unwrap();
    let mut buf = [0u8; TRANSFER_LEN];
    let first = down.fill(&mut buf, Header::default());
    let sent = sent_records(&buf, first);
    // The node took nothing, so the P4 never calls `sent`; a record queued since waits.
    down.push(6, 2, &[]).unwrap();
    assert!(down.has_work());
    assert_eq!(down.deadline_ms(), None, "nothing went out to wait on");
    let len = down.fill(&mut buf, Header::default());
    assert_eq!(sent_records(&buf, len), sent);
    down.sent(2, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(2)), 3).unwrap(), [5]);
}

#[test]
fn downlink_ignores_sent_when_ack_delivered_the_resend_first() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    down.push(4, 1, b"assign").unwrap();
    let mut buf = [0u8; TRANSFER_LEN];
    down.fill(&mut buf, Header::default());
    down.sent(1, 0);
    assert_eq!(down.poll(ACK_TIMEOUT_MS), None);
    assert!(down.fill(&mut buf, Header::default()) > 0, "the resend is due");
    // The P4 reads the node's header before marking the resend sent; it acks the first try.
    assert_eq!(down.on_header(&Header::new(0, Some(1)), 2).unwrap(), [4]);
    down.sent(2, ACK_TIMEOUT_MS);
    assert_eq!(down.deadline_ms(), None);
    assert!(!down.has_work());
}

#[test]
fn downlink_refuses_record_when_queue_full() {
    let mut down = Downlink::<64>::new();
    down.push(2, 1, &[0; 40]).unwrap();
    assert_eq!(down.push(3, 1, &[0; 40]), Err(Undelivered::QueueFull));
    assert_eq!(down.refused(), 1);
}

#[test]
fn downlink_drains_flight_and_queue_when_node_gone() {
    let mut down = Downlink::<FLIGHT_BYTES>::new();
    for id in 0..3 {
        down.push(id, 4, &[0; MAX_NODE_DOWN]).unwrap();
    }
    let mut buf = [0u8; TRANSFER_LEN];
    down.fill(&mut buf, Header::default());
    down.sent(1, 0);
    let mut ids = Vec::new();
    down.drain(|id| ids.push(id));
    assert_eq!(ids, [0, 1, 2]);
    assert!(!down.has_work());
    assert_eq!(down.deadline_ms(), None);
}
