//! The ring that keeps a node from reporting the same access point forever.

use wartui_proto::dedup::MacRing;
use wartui_proto::plan::{DEDUP_REFRESH_MS, DEDUP_RING, DEDUP_RSSI_GAIN_DB};

const T0: u32 = 1_000;

fn mac(n: u16) -> [u8; 6] {
    let [hi, lo] = n.to_be_bytes();
    [0x02, 0, 0, 0, hi, lo]
}

#[test]
fn the_first_sighting_is_reported_and_the_second_is_not() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-70), T0), "a new address is worth transmitting");
    assert!(!ring.offer(mac(1), Some(-70), T0 + 1), "the same address is not");
    assert!(ring.contains(&mac(1)));
    assert_eq!(ring.len(), 1);
}

#[test]
fn asking_does_not_record() {
    // The node asks, broadcasts, and records only if the broadcast went out.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.is_due(&mac(1), Some(-70), T0));
    assert!(ring.is_due(&mac(1), Some(-70), T0), "nothing was held by asking");
    ring.record(mac(1), Some(-70), T0);
    assert!(!ring.is_due(&mac(1), Some(-70), T0));
}

#[test]
fn an_address_is_reported_again_once_it_has_been_pushed_out() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(0), Some(-70), T0));
    for n in 1..=4 {
        assert!(ring.offer(mac(n), Some(-70), T0));
    }
    assert!(!ring.contains(&mac(0)), "the oldest entry is gone");
    assert!(ring.offer(mac(0), Some(-70), T0), "so it is worth transmitting again");
}

#[test]
fn a_stationary_node_reports_its_neighbourhood_again_after_the_refresh() {
    // Without this a node that has reported everything in range is silent for good,
    // across host sessions too.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-70), T0));
    assert!(!ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS - 1));
    assert!(ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS));
    assert!(!ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS + 1), "and the timer restarts");
}

#[test]
fn the_refresh_survives_the_millisecond_clock_wrapping() {
    let mut ring: MacRing<4> = MacRing::new();
    let before_wrap = u32::MAX - 10;
    assert!(ring.offer(mac(1), Some(-70), before_wrap));
    assert!(!ring.offer(mac(1), Some(-70), 5), "sixteen milliseconds later, not 49 days earlier");
    assert!(ring.offer(mac(1), Some(-70), before_wrap.wrapping_add(DEDUP_REFRESH_MS)));
}

#[test]
fn a_much_stronger_signal_is_reported_again_before_the_refresh() {
    // The export keeps the strongest sighting's position, so getting closer is news.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-80), T0));
    assert!(
        !ring.offer(mac(1), Some(-80 + DEDUP_RSSI_GAIN_DB - 1), T0 + 1),
        "under the margin is not"
    );
    assert!(ring.offer(mac(1), Some(-80 + DEDUP_RSSI_GAIN_DB), T0 + 2));
}

#[test]
fn the_signal_baseline_only_rises_so_wobbling_is_not_news() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-60), T0));
    assert!(!ring.offer(mac(1), Some(-90), T0 + 1));
    assert!(
        !ring.offer(mac(1), Some(-60 + DEDUP_RSSI_GAIN_DB - 1), T0 + 2),
        "measured against the strongest reported, not the weakest heard"
    );
}

#[test]
fn a_refresh_resets_the_signal_baseline() {
    // The node may be somewhere else entirely by now.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-40), T0));
    let later = T0 + DEDUP_REFRESH_MS;
    assert!(ring.offer(mac(1), Some(-90), later));
    assert!(ring.offer(mac(1), Some(-90 + DEDUP_RSSI_GAIN_DB), later + 1));
}

#[test]
fn a_reading_without_a_signal_strength_is_never_stronger() {
    // A BLE controller with no reading says 127, which must not read as +127 dBm.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), None, T0));
    assert!(!ring.offer(mac(1), None, T0 + 1));
    assert!(ring.offer(mac(1), Some(-100), T0 + 2), "any real reading beats no reading");
}

#[test]
fn a_repeat_does_not_move_an_address_back_to_the_front() {
    // A re-report updates in place, so a constantly-beaconing access point still ages
    // out on schedule.
    let mut ring: MacRing<3> = MacRing::new();
    ring.offer(mac(1), Some(-80), T0);
    ring.offer(mac(2), Some(-80), T0);
    assert!(ring.offer(mac(1), Some(-50), T0 + 1), "re-reported for its signal");
    ring.offer(mac(3), Some(-80), T0);
    ring.offer(mac(4), Some(-80), T0);
    assert!(!ring.contains(&mac(1)), "the re-report bought it nothing");
}

#[test]
fn the_all_zero_address_is_not_seen_from_boot() {
    // A zeroed array with no length alongside it would read 00:00:00:00:00:00 as
    // already-reported; counting entries removes the special case.
    let ring: MacRing<8> = MacRing::new();
    assert!(ring.is_empty());
    assert!(!ring.contains(&[0; 6]));
    assert!(ring.is_due(&[0; 6], Some(-70), 0));
}

#[test]
fn the_firmware_sized_ring_holds_what_it_says_it_does() {
    let mut ring: MacRing<DEDUP_RING> = MacRing::new();
    for n in 0..u16::try_from(DEDUP_RING).expect("fits") {
        assert!(ring.offer(mac(n), Some(-70), T0), "every address here is distinct");
    }
    assert_eq!(ring.len(), DEDUP_RING);
    assert!(ring.contains(&mac(0)), "nothing has displaced the first entry yet");
    ring.offer(mac(9999), Some(-70), T0);
    assert!(!ring.contains(&mac(0)), "one more address displaces it");
    assert_eq!(ring.len(), DEDUP_RING, "and the ring does not grow");
}
