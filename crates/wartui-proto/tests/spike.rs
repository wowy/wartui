//! The Bluetooth host spike's load generator, its bodies and their CRC, and the C6's transmit
//! buffer and delivery ledger.

use wartui_proto::spike::{
    AclLedger, ByteRing, DOWN_UUID, FULL_BODY, H4_ACL, H4_EVENT, NAME_PREFIX, SERVICE_UUID,
    SMALL_BODY, SpikeProfile, UP_UUID, body_checks, crc32, fill_body,
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
