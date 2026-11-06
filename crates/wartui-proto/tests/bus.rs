//! The node bus's transfer framing and the records it carries.
//!
//! The vectors are written out by hand from the layout table in `bus/mod.rs`. Each CRC was
//! worked out with a separate bitwise CRC-16/CCITT-FALSE, checked against the standard
//! "123456789" value 0x29B1.

use wartui_proto::bus::{
    BUS_VERSION, CRC_OFFSET, HEADER_LEN, Header, MAGIC, MORE, PAYLOAD_MAX, Packer,
    RECORD_HEADER_LEN, RECORD_MAX, RecordType, SeqCheck, SeqNote, TRANSFER_LEN, VerifyError,
    records, seal, verify,
};
use wartui_proto::link::{MAX_NODE_PAYLOAD, crc16};

/// An idle transfer: no records.
#[rustfmt::skip]
const IDLE_HEADER: [u8; HEADER_LEN] = [
    0x57, 0x42, // magic "WB"
    0x01, // bus version 1
    0x00, // flags: nothing more queued
    0x02, 0x01, // seq 0x0102, little-endian
    0x04, 0x03, // ack 0x0304, little-endian
    0x00, 0x00, // len 0
];
/// CRC-16/CCITT-FALSE 0xB69C over the header and 1,028 zero bytes, little-endian.
const IDLE_CRC: [u8; 2] = [0x9C, 0xB6];

/// A transfer carrying one node frame, with more queued behind it.
#[rustfmt::skip]
const NODE_FRAME: [u8; HEADER_LEN + 7] = [
    0x57, 0x42, // magic "WB"
    0x01, // bus version 1
    0x01, // flags: MORE
    0x07, 0x00, // seq 7
    0x05, 0x00, // ack 5
    0x07, 0x00, // len 7: one record
    0x01, // record type 1: node frame
    0x04, 0x00, // record len 4
    0xDE, 0xAD, 0xBE, 0xEF, // the body, opaque to this layer
];
/// CRC-16/CCITT-FALSE 0xA203 over those bytes and the zero padding, little-endian.
const NODE_FRAME_CRC: [u8; 2] = [0x03, 0xA2];

fn idle(seq: u16) -> [u8; TRANSFER_LEN] {
    let mut buf = [0u8; TRANSFER_LEN];
    seal(&mut buf, Header { flags: 0, seq, ack: 0 }, 0);
    buf
}

fn rewrite_crc(buf: &mut [u8; TRANSFER_LEN]) {
    let crc = crc16(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn bus_has_pinned_sizes() {
    assert_eq!(
        (MAGIC, BUS_VERSION, TRANSFER_LEN, HEADER_LEN, PAYLOAD_MAX, RECORD_MAX),
        (0x4257, 1, 1040, 10, 1028, 1025)
    );
    assert_eq!(TRANSFER_LEN % 4, 0);
}

#[test]
fn bus_seals_pinned_bytes_when_idle() {
    // Junk where the payload goes: `seal` zeroes what nothing wrote.
    let mut buf = [0xAAu8; TRANSFER_LEN];
    seal(&mut buf, Header { flags: 0, seq: 0x0102, ack: 0x0304 }, 0);
    assert_eq!(buf[..HEADER_LEN], IDLE_HEADER);
    assert!(buf[HEADER_LEN..CRC_OFFSET].iter().all(|&b| b == 0));
    assert_eq!(buf[CRC_OFFSET..], IDLE_CRC);
}

#[test]
fn bus_seals_pinned_bytes_when_node_frame_packed() {
    let mut buf = [0xAAu8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(RecordType::NodeFrame, &[0xDE, 0xAD, 0xBE, 0xEF]));
    packer.seal(Header { flags: MORE, seq: 7, ack: 5 });
    assert_eq!(buf[..NODE_FRAME.len()], NODE_FRAME);
    assert!(buf[NODE_FRAME.len()..CRC_OFFSET].iter().all(|&b| b == 0));
    assert_eq!(buf[CRC_OFFSET..], NODE_FRAME_CRC);
}

#[test]
fn bus_round_trips_records_when_several_packed() {
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(RecordType::NodeFrame, b"first"));
    assert!(packer.push(RecordType::NodeFrame, b""));
    assert!(packer.push(RecordType::NodeFrame, b"third"));
    let len = packer.len();
    packer.seal(Header { flags: MORE, seq: 0xFFFF, ack: 0x1234 });

    let transfer = verify(&buf).unwrap();
    assert_eq!(transfer.header, Header { flags: MORE, seq: 0xFFFF, ack: 0x1234 });
    assert!(transfer.header.more());
    assert_eq!(transfer.payload.len(), len);
    let bodies: Vec<&[u8]> = transfer
        .records()
        .inspect(|r| assert_eq!(r.record_type(), Some(RecordType::NodeFrame)))
        .map(|r| r.body)
        .collect();
    assert_eq!(bodies, [&b"first"[..], b"", b"third"]);
}

#[test]
fn bus_round_trips_empty_payload_when_idle() {
    let buf = idle(3);
    let transfer = verify(&buf).unwrap();
    assert_eq!(transfer.header, Header { flags: 0, seq: 3, ack: 0 });
    assert!(!transfer.header.more());
    assert_eq!(transfer.records().count(), 0);
}

#[test]
fn bus_carries_max_node_frame_when_transfer_empty() {
    const { assert!(RECORD_MAX >= MAX_NODE_PAYLOAD) };
    let frame = [0x5Au8; MAX_NODE_PAYLOAD];
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(packer.push(RecordType::NodeFrame, &frame));
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
    assert!(packer.push(RecordType::NodeFrame, &[1; 1000]));
    let len = packer.len();
    // One byte of body past what is left: refused, not truncated, and nothing written.
    let left = PAYLOAD_MAX - len - RECORD_HEADER_LEN;
    assert!(!packer.fits(left + 1));
    assert!(!packer.push(RecordType::NodeFrame, &vec![2; left + 1]));
    assert_eq!(packer.len(), len);
    assert!(packer.push(RecordType::NodeFrame, &vec![3; left]));
    assert_eq!(packer.len(), PAYLOAD_MAX);
}

#[test]
fn bus_refuses_record_when_longer_than_record_max() {
    let mut buf = [0u8; TRANSFER_LEN];
    let mut packer = Packer::new(&mut buf);
    assert!(!packer.push(RecordType::NodeFrame, &[0; RECORD_MAX + 1]));
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

#[test]
fn bus_yields_record_when_type_unknown() {
    // Skipping a record is the receiver's choice, so the iterator hands it over.
    let payload = [9, 1, 0, 0xAA, 1, 1, 0, 0xBB];
    let all: Vec<(u8, Option<RecordType>)> =
        records(&payload).map(|r| (r.kind, r.record_type())).collect();
    assert_eq!(all, [(9, None), (1, Some(RecordType::NodeFrame))]);
}

#[test]
fn bus_notes_first_when_nothing_seen() {
    let mut check = SeqCheck::new();
    assert_eq!(check.ack(), 0);
    assert_eq!(check.note(40), SeqNote::First);
    assert_eq!(check.ack(), 40);
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
