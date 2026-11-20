//! The Bluetooth host link's GATT identity and the C6's notify buffer.

use wartui_proto::ble::{
    ByteRing, DOWN_UUID, ID_UUID, NAME_PREFIX, SERVICE_UUID, UP_UUID, advertised_octets,
};
use wartui_proto::wardrive_esp::beacon::{self, Beacon, STATUS_MAX};

#[test]
fn ble_uuids_stay_pinned_when_built() {
    // The C6 firmware and every host read these; a change strands whichever was not rebuilt.
    assert_eq!(SERVICE_UUID, 0xf241_0001_d870_4eb6_a5a0_ed64_5732_a877);
    assert_eq!(UP_UUID, 0xf241_0002_d870_4eb6_a5a0_ed64_5732_a877);
    assert_eq!(DOWN_UUID, 0xf241_0003_d870_4eb6_a5a0_ed64_5732_a877);
    assert_eq!(NAME_PREFIX, "wartui ");
    assert_eq!(ID_UUID, 0x5754);
}

/// The free board's status bytes for the bridge `44:C0`.
fn free_status() -> Vec<u8> {
    let mut out = [0; STATUS_MAX];
    let len = beacon::status(&mut out, &Beacon::default(), [0x44, 0xC0], false);
    out[..len].to_vec()
}

#[test]
fn advertised_octets_reads_id_when_it_holds_two_bytes() {
    assert_eq!(advertised_octets(Some(&[0x44, 0xC0]), None), Some([0x44, 0xC0]));
    // The id outranks a status beside it.
    let status = free_status();
    assert_eq!(advertised_octets(Some(&[0x96, 0xC4]), Some(&status)), Some([0x96, 0xC4]));
}

#[test]
fn advertised_octets_reads_status_when_id_is_empty() {
    // The phones' pair: an empty id beside the status's fixed bytes.
    let status = free_status();
    assert_eq!(advertised_octets(Some(&[]), Some(&status)), Some([0x44, 0xC0]));
}

#[test]
fn advertised_octets_reads_status_when_empty_id_is_beside_linked_status() {
    // The status beacon while a central is connected: the status carries the app's nickname.
    let mut nickname = heapless::Vec::new();
    nickname.extend_from_slice(b"Pixel 8 pocket").unwrap();
    let linked = Beacon { nickname, ..Beacon::default() };
    let mut out = [0; STATUS_MAX];
    let len = beacon::status(&mut out, &linked, [0x44, 0xC0], true);
    assert_eq!(len, 9 + 14);
    assert_eq!(advertised_octets(Some(&[]), Some(&out[..len])), Some([0x44, 0xC0]));
}

#[test]
fn advertised_octets_finds_none_when_empty_id_has_no_status() {
    assert_eq!(advertised_octets(Some(&[]), None), None);
    let status = free_status();
    assert_eq!(advertised_octets(Some(&[]), Some(&status[..8])), None);
    let mut unmarked = status.clone();
    unmarked[0] = b'X';
    assert_eq!(advertised_octets(Some(&[]), Some(&unmarked)), None);
}

#[test]
fn advertised_octets_finds_none_when_id_is_absent() {
    // A WardriveESP board that is not a bridge carries the status and no id.
    let status = free_status();
    assert_eq!(advertised_octets(None, Some(&status)), None);
    assert_eq!(advertised_octets(None, None), None);
}

#[test]
fn advertised_octets_finds_none_when_id_is_another_length() {
    let status = free_status();
    assert_eq!(advertised_octets(Some(&[0x44]), Some(&status)), None);
    assert_eq!(advertised_octets(Some(&[0x44, 0xC0, 0x01]), Some(&status)), None);
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

#[test]
fn byte_ring_stops_after_delimiter_when_taking_through() {
    let mut ring = ByteRing::<16>::new();
    ring.push(b"caps\nstop\n");
    let mut out = [0u8; 16];
    assert_eq!(ring.take_through(&mut out, b'\n'), 5);
    assert_eq!(&out[..5], b"caps\n");
    assert_eq!(ring.take_through(&mut out, b'\n'), 5);
    assert_eq!(&out[..5], b"stop\n");
    assert!(ring.is_empty());
    // Nothing held: nothing taken.
    assert_eq!(ring.take_through(&mut out, b'\n'), 0);
}

#[test]
fn byte_ring_fills_out_when_no_delimiter_is_held() {
    let mut ring = ByteRing::<16>::new();
    ring.push(b"abcdefgh");
    let mut out = [0u8; 3];
    assert_eq!(ring.take_through(&mut out, b'\n'), 3);
    assert_eq!(&out, b"abc");
    // Fewer held than `out` holds: all of it.
    let mut out = [0u8; 8];
    assert_eq!(ring.take_through(&mut out, b'\n'), 5);
    assert_eq!(&out[..5], b"defgh");
    // A delimiter past `out`'s length is not reached.
    ring.push(b"wxyz\n");
    let mut out = [0u8; 2];
    assert_eq!(ring.take_through(&mut out, b'\n'), 2);
    assert_eq!(ring.len(), 3);
}

#[test]
fn byte_ring_finds_delimiter_when_bytes_wrap() {
    let mut ring = ByteRing::<8>::new();
    ring.push(b"123456");
    let mut out = [0u8; 8];
    assert_eq!(ring.take(&mut out[..5]), 5);
    // Starts at index 5: the `\n` lands at index 1 after wrapping.
    assert_eq!(ring.push(b"6ab\nc"), 5);
    assert_eq!(ring.take_through(&mut out, b'\n'), 5);
    assert_eq!(&out[..5], b"66ab\n");
    assert_eq!(ring.take_through(&mut out, b'\n'), 1);
    assert_eq!(out[0], b'c');
}
