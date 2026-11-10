//! The Bluetooth host spike's load generator: its bodies and their CRC.

use wartui_proto::spike::{FULL_BODY, SMALL_BODY, SpikeProfile, body_checks, crc32, fill_body};

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
