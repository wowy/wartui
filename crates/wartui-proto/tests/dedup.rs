//! The ring that keeps a node from reporting the same access point forever.

use wartui_proto::dedup::MacRing;
use wartui_proto::plan::DEDUP_RING;

fn mac(n: u16) -> [u8; 6] {
    let [hi, lo] = n.to_be_bytes();
    [0x02, 0, 0, 0, hi, lo]
}

#[test]
fn the_first_sighting_is_reported_and_the_second_is_not() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.insert(mac(1)), "a new address is worth transmitting");
    assert!(!ring.insert(mac(1)), "the same address is not");
    assert!(ring.contains(&mac(1)));
    assert_eq!(ring.len(), 1);
}

#[test]
fn an_address_is_reported_again_once_it_has_been_pushed_out() {
    // This is what keeps a stationary node's observation stream alive rather
    // than dying completely after the first pass.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.insert(mac(0)));
    for n in 1..=4 {
        assert!(ring.insert(mac(n)));
    }
    assert!(!ring.contains(&mac(0)), "the oldest entry is gone");
    assert!(ring.insert(mac(0)), "so it is worth transmitting again");
}

#[test]
fn a_repeat_does_not_move_an_address_back_to_the_front() {
    // A lookup returns before touching the cursor,
    // so a constantly-beaconing access point still ages out on schedule.
    let mut ring: MacRing<3> = MacRing::new();
    ring.insert(mac(1));
    ring.insert(mac(2));
    assert!(!ring.insert(mac(1)), "already held");
    ring.insert(mac(3));
    ring.insert(mac(4));
    assert!(!ring.contains(&mac(1)), "the repeat bought it nothing");
}

#[test]
fn the_all_zero_address_is_not_seen_from_boot() {
    // A zeroed array with no length alongside it reads 00:00:00:00:00:00 as
    // already-reported; counting entries removes the special case.
    let ring: MacRing<8> = MacRing::new();
    assert!(ring.is_empty());
    assert!(!ring.contains(&[0; 6]));
}

#[test]
fn the_firmware_sized_ring_holds_what_it_says_it_does() {
    let mut ring: MacRing<DEDUP_RING> = MacRing::new();
    for n in 0..u16::try_from(DEDUP_RING).expect("fits") {
        assert!(ring.insert(mac(n)), "every address here is distinct");
    }
    assert_eq!(ring.len(), DEDUP_RING);
    assert!(ring.contains(&mac(0)), "nothing has displaced the first entry yet");
    ring.insert(mac(9999));
    assert!(!ring.contains(&mac(0)), "one more address displaces it");
    assert_eq!(ring.len(), DEDUP_RING, "and the ring does not grow");
}
