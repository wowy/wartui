//! The CRC-32 the P4's flash records end in.

use wartui_proto::crc::crc32;

#[test]
fn crc32_matches_iso_hdlc_check_value_when_given_test_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(&[]), 0);
}
