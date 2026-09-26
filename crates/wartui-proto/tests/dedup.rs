//! The ring that keeps a node from reporting the same access point forever.

use wartui_proto::dedup::{DedupRing, MacRing};
use wartui_proto::plan::{DEDUP_REFRESH_MS, DEDUP_RING, DEDUP_RSSI_GAIN_DB};

const T0: u32 = 1_000;

fn mac(n: u16) -> [u8; 6] {
    let [hi, lo] = n.to_be_bytes();
    [0x02, 0, 0, 0, hi, lo]
}

#[test]
fn mac_ring_accepts_first_sighting_and_filters_immediate_duplicate_when_offered() {
    let mut ring: MacRing<4, 8> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-70), T0), "a new address is worth transmitting");
    assert!(!ring.offer(mac(1), Some(-70), T0 + 1), "the same address is not");
    assert!(ring.contains(&mac(1)));
    assert_eq!(ring.len(), 1);
}

#[test]
fn mac_ring_preserves_unrecorded_state_when_only_querying_is_due() {
    // The node asks, broadcasts, and records only if the broadcast went out.
    let mut ring: MacRing<4, 8> = MacRing::new();
    assert!(ring.is_due(&mac(1), Some(-70), T0));
    assert!(ring.is_due(&mac(1), Some(-70), T0), "nothing was held by asking");
    ring.record(mac(1), Some(-70), T0);
    assert!(!ring.is_due(&mac(1), Some(-70), T0));
}

#[test]
fn mac_ring_permits_retransmission_when_old_address_is_evicted() {
    let mut ring: MacRing<4, 8> = MacRing::new();
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
    let mut ring: MacRing<4, 8> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-70), T0));
    assert!(!ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS - 1));
    assert!(ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS));
    assert!(!ring.offer(mac(1), Some(-70), T0 + DEDUP_REFRESH_MS + 1), "and the timer restarts");
}

#[test]
fn mac_ring_handles_refresh_correctly_when_millisecond_clock_wraps() {
    let mut ring: MacRing<4, 8> = MacRing::new();
    let before_wrap = u32::MAX - 10;
    assert!(ring.offer(mac(1), Some(-70), before_wrap));
    assert!(!ring.offer(mac(1), Some(-70), 5), "sixteen milliseconds later, not 49 days earlier");
    assert!(ring.offer(mac(1), Some(-70), before_wrap.wrapping_add(DEDUP_REFRESH_MS)));
}

#[test]
fn mac_ring_accepts_duplicate_address_when_rssi_gain_exceeds_threshold() {
    // The export keeps the strongest sighting's position, so getting closer is news.
    let mut ring: MacRing<4, 8> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-80), T0));
    assert!(!ring.offer(mac(1), Some(-80 + DEDUP_RSSI_GAIN_DB - 1), T0 + 1), "jitter is not");
    assert!(ring.offer(mac(1), Some(-80 + DEDUP_RSSI_GAIN_DB), T0 + 2));
}

#[test]
fn mac_ring_filters_weaker_signals_when_rssi_fluctuates_below_peak() {
    let mut ring: MacRing<4, 8> = MacRing::new();
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
    let mut ring: MacRing<4, 8> = MacRing::new();
    assert!(ring.offer(mac(1), Some(-40), T0));
    let later = T0 + DEDUP_REFRESH_MS;
    assert!(ring.offer(mac(1), Some(-90), later));
    assert!(ring.offer(mac(1), Some(-90 + DEDUP_RSSI_GAIN_DB), later + 1));
}

#[test]
fn mac_ring_prefers_valid_rssi_over_missing_signal_when_evaluating_due() {
    // A BLE controller with no reading says 127, which must not read as +127 dBm.
    let mut ring: MacRing<4, 8> = MacRing::new();
    assert!(ring.offer(mac(1), None, T0));
    assert!(!ring.offer(mac(1), None, T0 + 1));
    assert!(ring.offer(mac(1), Some(-100), T0 + 2), "any real reading beats no reading");
}

#[test]
fn mac_ring_preserves_fifo_eviction_order_when_address_is_re_reported() {
    // A re-report updates in place, so a constantly-beaconing access point still ages
    // out on schedule.
    let mut ring: MacRing<3, 8> = MacRing::new();
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
    let ring: MacRing<8, 16> = MacRing::new();
    assert!(ring.is_empty());
    assert!(!ring.contains(&[0; 6]));
    assert!(ring.is_due(&[0; 6], Some(-70), 0));
}

#[test]
fn mac_ring_forgets_every_address_when_cleared() {
    let mut ring: MacRing<4, 8> = MacRing::new();
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
    let mut ring: MacRing<3, 8> = MacRing::new();
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
    let mut ring: DedupRing = MacRing::new();
    for n in 0..u16::try_from(DEDUP_RING).expect("fits") {
        assert!(ring.offer(mac(n), Some(-70), T0), "every address here is distinct");
    }
    assert_eq!(ring.len(), DEDUP_RING);
    assert!(ring.contains(&mac(0)), "nothing has displaced the first entry yet");
    ring.offer(mac(9999), Some(-70), T0);
    assert!(!ring.contains(&mac(0)), "one more address displaces it");
    assert_eq!(ring.len(), DEDUP_RING, "and the ring does not grow");
}

#[test]
fn mac_ring_removes_only_the_evicted_mac_from_the_index_when_ring_overflows() {
    let mut ring: MacRing<4, 8> = MacRing::new();
    for n in 0..4u16 {
        assert!(ring.offer(mac(n), Some(-70), T0));
    }
    assert!(ring.offer(mac(99), Some(-70), T0), "overflow by one");
    assert!(!ring.contains(&mac(0)), "the oldest is evicted, and unindexed with it");
    for n in 1..4u16 {
        assert!(ring.contains(&mac(n)), "mac {n} survives the eviction");
    }
    assert!(ring.contains(&mac(99)));
    assert_eq!(ring.len(), 4);
}

/// Multiplicative hash matching [`wartui_proto::dedup`]'s documented contract, used
/// here only to find MACs that collide — the ring itself never exposes it.
fn hash_for_test(mac: &[u8; 6], bits: u32) -> usize {
    let hi = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]);
    let lo = u32::from_be_bytes([0, 0, mac[4], mac[5]]);
    let h = (hi ^ lo).wrapping_mul(0x9E37_79B1);
    (h >> (32 - bits)) as usize
}

/// Three MACs landing in the same hash slot of a `MacRing<_, 8>`, in the order that
/// puts the first at the head of the probe run and the rest behind it.
fn colliding_macs() -> [[u8; 6]; 3] {
    let mut buckets: std::collections::HashMap<usize, Vec<[u8; 6]>> = Default::default();
    for n in 0..2_000u16 {
        let m = mac(n);
        buckets.entry(hash_for_test(&m, 3)).or_default().push(m);
    }
    let group =
        buckets.values().find(|v| v.len() >= 3).expect("some slot collides within 2000 MACs");
    [group[0], group[1], group[2]]
}

#[test]
fn mac_ring_finds_later_probe_entries_when_the_head_of_their_run_is_evicted() {
    // `a` claims the home slot; `b` and `c` land behind it in the same probe run.
    let [a, b, c] = colliding_macs();
    let mut ring: MacRing<4, 8> = MacRing::new();
    ring.offer(a, Some(-70), T0);
    ring.offer(b, Some(-70), T0);
    ring.offer(c, Some(-70), T0);
    ring.offer(mac(9999), Some(-70), T0);
    // Overflow evicts `a`, the oldest entry and the head of the run — the case
    // backward-shift deletion exists for.
    ring.offer(mac(9998), Some(-70), T0);
    assert!(!ring.contains(&a), "the head of the run is gone");
    assert!(ring.contains(&b), "later in the run, but still reachable from the home slot");
    assert!(ring.contains(&c), "same for the entry behind that one");
}

#[test]
fn mac_ring_forgets_everything_and_refills_cleanly_when_cleared_at_capacity() {
    let mut ring: MacRing<4, 8> = MacRing::new();
    for n in 0..4u16 {
        ring.offer(mac(n), Some(-70), T0);
    }
    ring.clear();
    for n in 0..4u16 {
        assert!(!ring.contains(&mac(n)), "mac {n} is gone");
    }
    for n in 100..104u16 {
        assert!(ring.offer(mac(n), Some(-70), T0), "refilling after clear is unobstructed");
    }
    assert_eq!(ring.len(), 4);
    for n in 100..104u16 {
        assert!(ring.contains(&mac(n)));
    }
}

/// One held address, in the naive model below.
#[derive(Clone, Copy)]
struct NaiveEntry {
    mac: [u8; 6],
    at_ms: u32,
    best_rssi: i8,
}

/// The pre-hash implementation this crate carried before the index was added,
/// reimplemented here as the model the hashed ring is checked against.
struct NaiveRing {
    entries: Vec<NaiveEntry>,
    cap: usize,
    cursor: usize,
}

impl NaiveRing {
    fn new(cap: usize) -> Self {
        Self { entries: Vec::new(), cap, cursor: 0 }
    }

    fn find(&self, mac: &[u8; 6]) -> Option<usize> {
        self.entries.iter().position(|e| e.mac == *mac)
    }

    fn contains(&self, mac: &[u8; 6]) -> bool {
        self.find(mac).is_some()
    }

    fn is_due(&self, mac: &[u8; 6], rssi: Option<i8>, now_ms: u32) -> bool {
        let Some(i) = self.find(mac) else { return true };
        let entry = &self.entries[i];
        now_ms.wrapping_sub(entry.at_ms) >= DEDUP_REFRESH_MS
            || rssi.is_some_and(|r| {
                i16::from(r) >= i16::from(entry.best_rssi) + i16::from(DEDUP_RSSI_GAIN_DB)
            })
    }

    fn record(&mut self, mac: [u8; 6], rssi: Option<i8>, now_ms: u32) {
        let rssi = rssi.unwrap_or(i8::MIN);
        if let Some(i) = self.find(&mac) {
            let entry = &mut self.entries[i];
            if now_ms.wrapping_sub(entry.at_ms) >= DEDUP_REFRESH_MS {
                *entry = NaiveEntry { mac, at_ms: now_ms, best_rssi: rssi };
            } else {
                entry.best_rssi = entry.best_rssi.max(rssi);
            }
            return;
        }
        let fresh = NaiveEntry { mac, at_ms: now_ms, best_rssi: rssi };
        if self.entries.len() < self.cap {
            self.entries.push(fresh);
        } else {
            self.entries[self.cursor] = fresh;
        }
        self.cursor = (self.cursor + 1) % self.cap;
    }

    fn offer(&mut self, mac: [u8; 6], rssi: Option<i8>, now_ms: u32) -> bool {
        let due = self.is_due(&mac, rssi, now_ms);
        if due {
            self.record(mac, rssi, now_ms);
        }
        due
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
    }
}

/// A minimal deterministic PRNG (MMIX's LCG), so the random test is reproducible
/// without a new dependency.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }
}

#[test]
fn mac_ring_matches_a_naive_model_when_driven_by_a_long_random_sequence() {
    const N: usize = 6;
    const ADDRESSES: u16 = 40;

    let mut ring: MacRing<N, 16> = MacRing::new();
    let mut model = NaiveRing::new(N);
    let mut rng = Lcg::new(0xD1CE_5EED);
    let mut now = T0;

    for step in 0..20_000u32 {
        if step % 4_001 == 4_000 {
            ring.clear();
            model.clear();
            continue;
        }
        let m = mac(rng.next_u32() as u16 % ADDRESSES);
        let rssi = if rng.next_u32().is_multiple_of(5) {
            None
        } else {
            Some((rng.next_u32() % 60) as i8 - 100)
        };
        now = now.wrapping_add(rng.next_u32() % 50);

        match rng.next_u32() % 3 {
            0 => assert_eq!(
                ring.contains(&m),
                model.contains(&m),
                "contains diverges at step {step}"
            ),
            1 => assert_eq!(
                ring.is_due(&m, rssi, now),
                model.is_due(&m, rssi, now),
                "is_due diverges at step {step}"
            ),
            _ => assert_eq!(
                ring.offer(m, rssi, now),
                model.offer(m, rssi, now),
                "offer diverges at step {step}"
            ),
        }
        assert_eq!(ring.len(), model.entries.len(), "len diverges at step {step}");
    }

    for n in 0..ADDRESSES {
        assert_eq!(
            ring.contains(&mac(n)),
            model.contains(&mac(n)),
            "final state diverges for mac {n}"
        );
    }
}
