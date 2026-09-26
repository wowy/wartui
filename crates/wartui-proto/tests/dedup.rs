//! The ring that keeps a node from reporting the same access point forever.

use wartui_proto::dedup::MacRing;
use wartui_proto::plan::{DEDUP_REFRESH_MS, DEDUP_RING, DEDUP_RSSI_GAIN_DB};

const T0: u32 = 1_000;

fn mac(n: u16) -> [u8; 6] {
    let [hi, lo] = n.to_be_bytes();
    [0x02, 0, 0, 0, hi, lo]
}

#[test]
fn mac_ring_accepts_first_sighting_and_filters_immediate_duplicate_when_offered() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-70), T0), "a new address is worth transmitting");
    assert!(!ring.offer(mac(1), Some(-70), T0 + 1), "the same address is not");
    assert!(ring.contains(&mac(1)));
    assert_eq!(ring.len(), 1);
}

#[test]
fn mac_ring_preserves_unrecorded_state_when_only_querying_is_due() {
    // The node asks, broadcasts, and records only if the broadcast went out.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.is_due(&mac(1), Some(-70), T0));
    assert!(ring.is_due(&mac(1), Some(-70), T0), "nothing was held by asking");
    ring.record(mac(1), Some(-70), T0);
    assert!(!ring.is_due(&mac(1), Some(-70), T0));
}

#[test]
fn mac_ring_permits_retransmission_when_old_address_is_evicted() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(0), Some(-70), T0));
    for n in 1..=4 {
        assert!(ring.offer(mac(n), Some(-70), T0));
    }
    assert!(!ring.contains(&mac(0)), "the oldest entry is gone");
    assert!(ring.offer(mac(0), Some(-70), T0), "so it is worth transmitting again");
}

#[test]
fn mac_ring_permits_retransmission_when_refresh_window_elapses() {
    // Without this a node that has reported everything in range is silent for good,
    // across host sessions too.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-70), T0));
    assert!(!ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS - 1));
    assert!(ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS));
    assert!(!ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS + 1), "and the timer restarts");
}

#[test]
fn mac_ring_handles_refresh_correctly_when_millisecond_clock_wraps() {
    let mut ring: MacRing<4> = MacRing::new();
    let before_wrap = u32::MAX - 10;
    assert!(ring.offer(mac(1), Some(-70), before_wrap));
    assert!(!ring.offer(mac(1), Some(-70), 5), "sixteen milliseconds later, not 49 days earlier");
    assert!(ring.offer(mac(1), Some(-70), before_wrap.wrapping_add(DEDUP_REFRESH_MS)));
}

#[test]
fn mac_ring_accepts_duplicate_address_when_rssi_gain_exceeds_threshold() {
    // The export keeps the strongest sighting's position, so getting closer is news.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-80), T0));
    assert!(!ring.offer(mac(1), Some(-80 + DEDUP_RSSI_GAIN_DB - 1), T0 + 1), "jitter is not");
    assert!(ring.offer(mac(1), Some(-80 + DEDUP_RSSI_GAIN_DB), T0 + 2));
}

#[test]
fn mac_ring_filters_weaker_signals_when_rssi_fluctuates_below_peak() {
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-60), T0));
    assert!(!ring.offer(mac(1), Some(-90), T0 + 1));
    assert!(
        !ring.offer(mac(1), Some(-60 + DEDUP_RSSI_GAIN_DB - 1), T0 + 2),
        "measured against the strongest reported, not the weakest heard"
    );
}

#[test]
fn mac_ring_resets_rssi_baseline_when_refresh_interval_elapses() {
    // The node may be somewhere else entirely by now.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-40), T0));
    let later = T0 + DEDUP_REFRESH_MS;
    assert!(ring.offer(mac(1), Some(-90), later));
    assert!(ring.offer(mac(1), Some(-90 + DEDUP_RSSI_GAIN_DB), later + 1));
}

#[test]
fn mac_ring_prefers_valid_rssi_over_missing_signal_when_evaluating_due() {
    // A BLE controller with no reading says 127, which must not read as +127 dBm.
    let mut ring: MacRing<4> = MacRing::new();
    assert!(ring.offer(mac(1), None, T0));
    assert!(!ring.offer(mac(1), None, T0 + 1));
    assert!(ring.offer(mac(1), Some(-100), T0 + 2), "any real reading beats no reading");
}

#[test]
fn mac_ring_preserves_fifo_eviction_order_when_address_is_re_reported() {
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
fn mac_ring_treats_zero_mac_as_unseen_when_ring_is_fresh() {
    // A zeroed array with no length alongside it would read 00:00:00:00:00:00 as
    // already-reported; counting entries removes the special case.
    let ring: MacRing<8> = MacRing::new();
    assert!(ring.is_empty());
    assert!(!ring.contains(&[0; 6]));
    assert!(ring.is_due(&[0; 6], Some(-70), 0));
}

#[test]
fn mac_ring_forgets_every_address_when_cleared() {
    let mut ring: MacRing<4> = MacRing::new();
    ring.offer(mac(1), Some(-70), T0);
    ring.offer(mac(2), Some(-70), T0);
    ring.clear();
    assert_eq!(ring.len(), 0);
    assert!(ring.is_empty());
    assert!(!ring.contains(&mac(1)));
    assert!(ring.is_due(&mac(1), Some(-70), T0), "due again, not merely unrecorded");
}

#[test]
fn mac_ring_fills_from_the_start_when_recording_after_clear() {
    let mut ring: MacRing<3> = MacRing::new();
    ring.offer(mac(1), Some(-70), T0);
    ring.offer(mac(2), Some(-70), T0);
    ring.offer(mac(3), Some(-70), T0);
    ring.clear();
    ring.offer(mac(4), Some(-70), T0);
    assert_eq!(ring.len(), 1);
    assert!(ring.contains(&mac(4)));
    assert!(!ring.contains(&mac(1)), "the pre-clear entries are gone");
}

#[test]
fn mac_ring_enforces_capacity_bounds_when_configured_with_firmware_ring_size() {
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
