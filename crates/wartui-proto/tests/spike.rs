//! The Bluetooth host spike's load generator, its bodies and their CRC, the C6's transmit
//! buffer and delivery ledger, and its diagnosis of corrupted P4 packets.

use wartui_proto::radio_link::{BODY_MAX, HEADER_LEN, Kind, WIRE_MAX, encode_packet, seal};
use wartui_proto::spike::{
    AclLedger, ByteRing, DOWN_UUID, Diagnosis, FLIP_SEARCH_MAX, FULL_BODY, Fault, H4_ACL, H4_EVENT,
    Header, NAME_PREFIX, SERVICE_UUID, SMALL_BODY, SpikeProfile, UP_UUID, body_checks, crc32,
    diagnose, fill_body,
};

#[test]
fn spike_crc32_matches_iso_hdlc_check_value_when_given_test_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(&[]), 0);
}

#[test]
fn spike_body_checks_when_filled() {
    for (seq, len) in [(0, SMALL_BODY), (7, SMALL_BODY), (u32::MAX, FULL_BODY), (3, 4)] {
        let mut body = vec![0u8; len];
        fill_body(seq, &mut body);
        assert!(body_checks(&body), "seq {seq}, {len} bytes");
    }
}

#[test]
fn spike_body_fails_check_when_a_byte_flips() {
    let mut body = vec![0u8; SMALL_BODY];
    fill_body(9, &mut body);
    body[3] ^= 0x10;
    assert!(!body_checks(&body));
    assert!(!body_checks(&[1, 2, 3]));
}

#[test]
fn spike_body_differs_when_seq_differs() {
    let mut first = vec![0u8; SMALL_BODY];
    let mut second = vec![0u8; SMALL_BODY];
    fill_body(1, &mut first);
    fill_body(2, &mut second);
    assert_ne!(first, second);
}

#[test]
fn spike_profile_gives_rate_and_body_when_asked() {
    assert_eq!(SpikeProfile::Drive.per_10s(), Some(75));
    assert_eq!(SpikeProfile::Fleet20.per_10s(), Some(250));
    assert_eq!(SpikeProfile::Burst.per_10s(), Some(7_000));
    assert_eq!(SpikeProfile::Saturate.per_10s(), None);
    assert_eq!(SpikeProfile::Off.per_10s(), None);
    assert_eq!(SpikeProfile::Drive.body_len(), 45);
    assert_eq!(SpikeProfile::Saturate.body_len(), 1024);
    assert_eq!(SpikeProfile::Burst.body_len(), 1024);
}

#[test]
fn spike_uuids_stay_pinned_when_built() {
    // The C6 firmware and the host stand-in read these; a change strands whichever was not
    // rebuilt.
    assert_eq!(SERVICE_UUID, 0xf241_0001_d870_4eb6_a5a0_ed64_5732_a877);
    assert_eq!(UP_UUID, 0xf241_0002_d870_4eb6_a5a0_ed64_5732_a877);
    assert_eq!(DOWN_UUID, 0xf241_0003_d870_4eb6_a5a0_ed64_5732_a877);
    assert_eq!(NAME_PREFIX, "wartui ");
}

#[test]
fn byte_ring_returns_bytes_in_order_when_they_wrap() {
    let mut ring = ByteRing::<8>::new();
    assert_eq!(ring.push(&[1, 2, 3, 4, 5, 6]), 6);
    let mut out = [0u8; 4];
    assert_eq!(ring.take(&mut out), 4);
    assert_eq!(out, [1, 2, 3, 4]);
    // Starts at index 6, so this wraps.
    assert_eq!(ring.push(&[7, 8, 9, 10, 11]), 5);
    assert_eq!(ring.len(), 7);
    let mut out = [0u8; 10];
    assert_eq!(ring.take(&mut out), 7);
    assert_eq!(out[..7], [5, 6, 7, 8, 9, 10, 11]);
    assert!(ring.is_empty());
}

#[test]
fn byte_ring_takes_what_fits_when_full() {
    let mut ring = ByteRing::<4>::new();
    assert_eq!(ring.push(&[1, 2, 3, 4, 5, 6]), 4);
    assert_eq!(ring.push(&[7]), 0);
    ring.clear();
    assert!(ring.is_empty());
    assert_eq!(ring.push(&[8]), 1);
    let mut out = [0u8; 2];
    assert_eq!(ring.take(&mut out), 1);
    assert_eq!(out[0], 8);
}

/// An ACL header's bytes after the H4 type: the handle with packet-boundary flags set, then
/// the payload length.
fn acl(handle: u16, len: u16) -> [u8; 4] {
    let [lo, hi] = (handle | 0x2000).to_le_bytes();
    let [len_lo, len_hi] = len.to_le_bytes();
    [lo, hi, len_lo, len_hi]
}

/// A Number Of Completed Packets event's bytes after the H4 type.
fn completed(pairs: &[(u16, u16)]) -> Vec<u8> {
    let mut event = vec![0x13, (1 + 4 * pairs.len()) as u8, pairs.len() as u8];
    for (handle, count) in pairs {
        event.extend(handle.to_le_bytes());
        event.extend(count.to_le_bytes());
    }
    event
}

#[test]
fn acl_ledger_counts_delivered_bytes_when_controller_completes_packets() {
    let mut ledger = AclLedger::<8>::new();
    ledger.on_host_packet(H4_ACL, &acl(1, 251));
    ledger.on_host_packet(H4_ACL, &acl(1, 100));
    ledger.on_host_packet(H4_ACL, &acl(1, 7));
    assert_eq!(ledger.acl_bytes(), 358);
    assert_eq!(ledger.delivered_bytes(), 0);
    ledger.on_controller_packet(H4_EVENT, &completed(&[(1, 2)]));
    assert_eq!(ledger.delivered_bytes(), 351);
    assert_eq!(ledger.in_flight(), 1);
}

#[test]
fn acl_ledger_keeps_other_handles_when_one_completes() {
    let mut ledger = AclLedger::<8>::new();
    ledger.on_host_packet(H4_ACL, &acl(2, 10));
    ledger.on_host_packet(H4_ACL, &acl(1, 20));
    ledger.on_host_packet(H4_ACL, &acl(2, 30));
    ledger.on_controller_packet(H4_EVENT, &completed(&[(1, 1), (2, 1)]));
    assert_eq!(ledger.delivered_bytes(), 30);
    // Handle 2's oldest went first; its 30 is still held.
    ledger.on_controller_packet(H4_EVENT, &completed(&[(2, 1)]));
    assert_eq!(ledger.delivered_bytes(), 60);
    assert_eq!(ledger.in_flight(), 0);
}

#[test]
fn acl_ledger_forgets_handle_when_disconnection_completes() {
    let mut ledger = AclLedger::<8>::new();
    ledger.on_host_packet(H4_ACL, &acl(1, 10));
    ledger.on_host_packet(H4_ACL, &acl(3, 20));
    // Status 0, handle 1, reason 0x13.
    ledger.on_controller_packet(H4_EVENT, &[0x05, 4, 0, 1, 0, 0x13]);
    assert_eq!(ledger.in_flight(), 1);
    // Handle 1 reconnects, and its completions count only what it sent since.
    ledger.on_host_packet(H4_ACL, &acl(1, 5));
    ledger.on_controller_packet(H4_EVENT, &completed(&[(1, 1)]));
    assert_eq!(ledger.delivered_bytes(), 5);
}

#[test]
fn acl_ledger_counts_untracked_when_full() {
    let mut ledger = AclLedger::<2>::new();
    for _ in 0..3 {
        ledger.on_host_packet(H4_ACL, &acl(1, 10));
    }
    assert_eq!(ledger.untracked(), 1);
    assert_eq!(ledger.acl_bytes(), 30);
    ledger.on_controller_packet(H4_EVENT, &completed(&[(1, 3)]));
    assert_eq!(ledger.delivered_bytes(), 20);
}

#[test]
fn acl_ledger_ignores_packet_when_not_acl_or_completion() {
    let mut ledger = AclLedger::<4>::new();
    // A command, a Command Complete, a failed disconnection and a short completion event.
    ledger.on_host_packet(0x01, &[0x03, 0x0C, 0x00, 0x00]);
    ledger.on_host_packet(H4_ACL, &[1, 0]);
    ledger.on_host_packet(H4_ACL, &acl(1, 4));
    ledger.on_controller_packet(H4_EVENT, &[0x0E, 4, 1, 0x03, 0x0C, 0]);
    ledger.on_controller_packet(H4_EVENT, &[0x05, 4, 0x0C, 1, 0, 0x13]);
    ledger.on_controller_packet(H4_EVENT, &[0x13, 5, 1, 1, 0]);
    ledger.on_controller_packet(H4_ACL, &completed(&[(1, 1)]));
    assert_eq!(ledger.acl_bytes(), 4);
    assert_eq!(ledger.delivered_bytes(), 0);
    assert_eq!(ledger.in_flight(), 1);
}

const SEQ: u32 = 0x0102_0304;

/// A `Messages` packet of `len` payload bytes as it arrives: COBS-encoded, without its
/// terminator. Every ninth payload byte is zero, so the frame has several COBS blocks.
fn frame(len: usize) -> Vec<u8> {
    let mut body = vec![0u8; BODY_MAX];
    for (i, byte) in body[HEADER_LEN..HEADER_LEN + len].iter_mut().enumerate() {
        *byte = if i % 9 == 0 { 0 } else { (i * 7) as u8 | 1 };
    }
    let body_len = seal(&mut body, Kind::Messages, SEQ, len);
    let mut wire = vec![0u8; WIRE_MAX];
    let wire_len = encode_packet(&body[..body_len], &mut wire);
    wire.truncate(wire_len - 1);
    assert!(!wire.contains(&0));
    wire
}

fn diagnosed(raw: &[u8]) -> Diagnosis {
    let mut scratch = vec![0u8; raw.len()];
    diagnose(raw, &mut scratch)
}

#[test]
fn diagnose_finds_clean_when_frame_is_intact() {
    let raw = frame(40);
    assert_eq!(diagnosed(&raw), Diagnosis { raw_len: raw.len(), fault: Fault::Clean { seq: SEQ } });
}

#[test]
fn diagnose_finds_leading_stray_when_a_byte_precedes_the_frame() {
    let mut raw = frame(40);
    raw.insert(0, 0xFF);
    let diagnosis = diagnosed(&raw);
    assert_eq!(diagnosis.fault, Fault::LeadingStray { byte: 0xFF, seq: SEQ });
    assert_eq!(diagnosis.to_string(), format!("{} B, leading stray 0xff, seq {SEQ}", raw.len()));
}

#[test]
fn diagnose_finds_trailing_stray_when_a_byte_follows_the_frame() {
    let mut raw = frame(40);
    raw.push(0x5A);
    assert_eq!(diagnosed(&raw).fault, Fault::TrailingStray { byte: 0x5A, seq: SEQ });
}

#[test]
fn diagnose_finds_extra_byte_when_one_is_inserted_mid_frame() {
    let mut raw = frame(40);
    // Unlike either neighbor, so only removing it restores the frame.
    assert!(raw[19] != 0xC3 && raw[20] != 0xC3);
    raw.insert(20, 0xC3);
    let diagnosis = diagnosed(&raw);
    assert_eq!(diagnosis.fault, Fault::Extra { at: 20, byte: 0xC3, seq: SEQ });
    assert_eq!(diagnosis.to_string(), format!("{} B, extra 0xc3 at 20, seq {SEQ}", raw.len()));
}

#[test]
fn diagnose_finds_bit_flip_when_one_bit_is_wrong() {
    let mut raw = frame(40);
    raw[25] ^= 0x08;
    assert_ne!(raw[25], 0);
    let diagnosis = diagnosed(&raw);
    assert_eq!(diagnosis.fault, Fault::BitFlip { at: 25, bit: 3, seq: SEQ });
    assert_eq!(diagnosis.to_string(), format!("{} B, bit 3 flipped at 25, seq {SEQ}", raw.len()));
}

#[test]
fn diagnose_reports_header_longer_than_arrived_when_a_byte_is_dropped() {
    let mut raw = frame(40);
    raw.remove(25);
    let diagnosis = diagnosed(&raw);
    assert_eq!(diagnosis.fault, Fault::Unexplained { header: Some(Header { seq: SEQ, len: 40 }) });
    // The header wants 40 payload bytes, which would take more than arrived.
    assert!(raw.len() < 40 + 15);
    assert_eq!(
        diagnosis.to_string(),
        format!("{} B, unexplained, seq {SEQ} len 40 (>= 55 B)", raw.len())
    );
}

#[test]
fn diagnose_skips_bit_flip_search_when_frame_is_past_flip_search_max() {
    let mut raw = frame(FLIP_SEARCH_MAX);
    assert!(raw.len() > FLIP_SEARCH_MAX);
    raw[500] ^= 0x01;
    assert_ne!(raw[500], 0);
    let len = FLIP_SEARCH_MAX as u16;
    assert_eq!(
        diagnosed(&raw).fault,
        Fault::Unexplained { header: Some(Header { seq: SEQ, len }) }
    );
}

#[test]
fn diagnose_reports_no_header_when_frame_is_noise() {
    let raw = [0x03, 0xAA, 0xBB, 0x02, 0xCC];
    let diagnosis = diagnosed(&raw);
    assert_eq!(diagnosis.fault, Fault::Unexplained { header: None });
    assert_eq!(diagnosis.to_string(), "5 B, unexplained, no header");
    assert_eq!(diagnosed(&[]).fault, Fault::Unexplained { header: None });
}
