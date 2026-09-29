//! The buffer one dwell's Wi-Fi sightings wait in.
//!
//! Sightings are parsed from assembled beacons rather than built directly, since
//! `Sighting` has no public constructor and the parser is what the node uses.

use wartui_proto::beacon::{Sighting, WifiPending, parse_mgmt};
use wartui_proto::dedup::Refused;

/// A beacon from `bssid` naming `ssid`, parsed as heard at `rssi` on channel 6.
fn sighting(bssid: [u8; 6], ssid: &[u8], rssi: i8) -> Sighting {
    let mut frame = vec![0x80, 0x00, 0x00, 0x00];
    frame.extend_from_slice(&[0xFF; 6]); // addr1, destination
    frame.extend_from_slice(&bssid); // addr2, source
    frame.extend_from_slice(&bssid); // addr3, BSSID
    frame.extend_from_slice(&[0x00, 0x00]); // sequence control
    frame.extend_from_slice(&[0u8; 8]); // timestamp
    frame.extend_from_slice(&100u16.to_le_bytes()); // beacon interval
    frame.extend_from_slice(&0u16.to_le_bytes()); // capability: open
    frame.push(0); // SSID element
    frame.push(u8::try_from(ssid.len()).expect("SSID fits"));
    frame.extend_from_slice(ssid);
    parse_mgmt(&frame, rssi, 6).expect("a beacon")
}

fn bssid(n: u8) -> [u8; 6] {
    [0x02, 0, 0, 0, 0, n]
}

fn ap(n: u8, rssi: i8) -> Sighting {
    sighting(bssid(n), &[b'a', n], rssi)
}

fn due(_: &Sighting) -> bool {
    true
}

fn not_due(_: &Sighting) -> bool {
    false
}

fn drain<const N: usize, const S: usize>(pending: &mut WifiPending<N, S>) -> Vec<Sighting> {
    std::iter::from_fn(|| pending.take()).collect()
}

#[test]
fn wifi_pending_keeps_first_sighting_when_bssid_repeats() {
    let mut pending = WifiPending::<4, 8>::new();
    pending.record(sighting(bssid(1), b"first", -80), due);
    pending.record(sighting(bssid(1), b"second", -40), due);

    let held = drain(&mut pending);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].ssid(), b"first");
    assert_eq!(held[0].rssi, -80);
}

#[test]
fn wifi_pending_skips_sighting_without_counting_drop_when_not_due() {
    let mut pending = WifiPending::<1, 2>::new();
    pending.record(ap(1, -60), not_due);
    assert!(pending.is_empty());

    pending.record(ap(2, -60), due);
    pending.record(ap(3, -60), not_due);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.dropped(), 0, "a sighting not due is not turned away");
}

#[test]
fn wifi_pending_skips_due_check_when_bssid_already_held() {
    let mut pending = WifiPending::<4, 8>::new();
    pending.record(ap(1, -60), due);
    pending.record(ap(1, -50), |_| panic!("a held BSSID asks nothing"));
    assert_eq!(pending.len(), 1);
}

#[test]
fn wifi_pending_drops_newest_once_per_dwell_when_full() {
    let mut pending = WifiPending::<2, 4>::new();
    pending.record(ap(1, -60), due);
    pending.record(ap(2, -60), due);
    for _ in 0..3 {
        pending.record(ap(3, -60), due);
        pending.record(ap(4, -60), due);
    }
    assert_eq!(pending.dropped(), 2, "each address once, however often it beacons");
    assert_eq!(drain(&mut pending), [ap(1, -60), ap(2, -60)]);

    pending.clear();
    pending.record(ap(1, -60), due);
    pending.record(ap(2, -60), due);
    pending.record(ap(3, -60), due);
    assert_eq!(
        pending.dropped(),
        3,
        "a new dwell counts the address again, and the total carries on"
    );
}

#[test]
fn wifi_pending_treats_bssid_as_new_when_recorded_after_clear() {
    // The index is reset with the buffer: a stale slot would find the BSSID in last
    // dwell's leavings and ignore it without asking `due`.
    let mut pending = WifiPending::<4, 8>::new();
    pending.record(ap(1, -60), due);
    pending.record(ap(2, -60), due);
    pending.clear();

    pending.record(ap(2, -50), not_due);
    assert!(pending.is_empty(), "a new dwell asks `due` again");

    let mut asked = false;
    pending.record(ap(1, -70), |_| {
        asked = true;
        true
    });
    assert!(asked);
    assert_eq!(drain(&mut pending), [ap(1, -70)]);
}

#[test]
fn wifi_pending_stops_take_at_len_when_cleared() {
    let mut pending = WifiPending::<4, 8>::new();
    pending.record(ap(1, -60), due);
    pending.record(ap(2, -60), due);
    pending.clear();
    assert_eq!(pending.take(), None, "last dwell's leavings are not handed out");
}

/// Multiplicative hash matching the index `WifiPending` builds, used only to find
/// BSSIDs that collide.
fn hash_for_test(address: &[u8; 6], bits: u32) -> usize {
    let hi = u32::from_be_bytes([address[0], address[1], address[2], address[3]]);
    let lo = u32::from_be_bytes([0, 0, address[4], address[5]]);
    let h = (hi ^ lo).wrapping_mul(0x9E37_79B1);
    (h >> (32 - bits)) as usize
}

#[test]
fn wifi_pending_deduplicates_each_bssid_when_bssids_share_a_hash_slot() {
    let home = |n: u8| hash_for_test(&bssid(n), 3);
    let colliding: Vec<u8> = (0..=u8::MAX).filter(|&n| home(n) == home(0)).take(4).collect();
    assert_eq!(colliding.len(), 4, "four BSSIDs share a slot of eight");

    let mut pending = WifiPending::<4, 8>::new();
    for &n in &colliding {
        pending.record(ap(n, -80), due);
    }
    // Repeated in reverse so the lookups walk the probe run from its far end too.
    for &n in colliding.iter().rev() {
        pending.record(ap(n, -40), |_| panic!("{n} is held"));
    }
    let expected: Vec<Sighting> = colliding.iter().map(|&n| ap(n, -80)).collect();
    assert_eq!(drain(&mut pending), expected);
}

/// `WifiPending`'s rules over a linear search, the model the hashed buffer is checked
/// against. It counts drops with the same `Refused`, since what is under test is finding
/// the BSSID, not counting it.
struct NaivePending {
    items: Vec<Sighting>,
    cap: usize,
    taken: usize,
    dropped: Refused,
}

impl NaivePending {
    fn record(&mut self, sighting: Sighting, due: bool) {
        if self.items.iter().any(|s| s.bssid == sighting.bssid) {
            return;
        }
        if !due {
            return;
        }
        if self.items.len() == self.cap {
            self.dropped.note(&sighting.bssid);
            return;
        }
        self.items.push(sighting);
    }

    fn take(&mut self) -> Option<Sighting> {
        let sighting = self.items.get(self.taken).copied();
        self.taken += usize::from(sighting.is_some());
        sighting
    }

    fn clear(&mut self) {
        self.items.clear();
        self.taken = 0;
        self.dropped.reset();
    }
}

/// A minimal deterministic PRNG (MMIX's LCG), so the random test is reproducible
/// without a new dependency.
struct Lcg(u64);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }
}

#[test]
fn wifi_pending_matches_a_naive_model_when_driven_by_a_long_random_sequence() {
    const N: usize = 12;
    const ADDRESSES: u32 = 40;

    let mut pending = WifiPending::<N, 32>::new();
    let mut model = NaivePending { items: Vec::new(), cap: N, taken: 0, dropped: Refused::new() };
    let mut rng = Lcg(0x5EED_BEAC);

    for step in 0..5_000 {
        match rng.next_u32() % 40 {
            0 => {
                pending.clear();
                model.clear();
            }
            1..=4 => assert_eq!(pending.take(), model.take(), "take at step {step}"),
            _ => {
                let n = u8::try_from(rng.next_u32() % ADDRESSES).expect("fits");
                let rssi = i8::try_from(rng.next_u32() % 60).expect("fits") - 100;
                let is_due = !rng.next_u32().is_multiple_of(4);
                let sighting = ap(n, rssi);
                pending.record(sighting, |_| is_due);
                model.record(sighting, is_due);
            }
        }
        assert_eq!(pending.len(), model.items.len(), "len at step {step}");
        assert_eq!(pending.dropped(), model.dropped.total(), "dropped at step {step}");
    }
    assert!(model.dropped.total() > 50, "the sequence fills the buffer often enough to matter");
    assert_eq!(drain(&mut pending), std::iter::from_fn(|| model.take()).collect::<Vec<_>>());
}
