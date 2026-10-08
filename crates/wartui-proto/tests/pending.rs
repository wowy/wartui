//! The buffers one dwell's sightings and one scan's reports wait in, and the count of
//! what a full one turns away.

mod common;

use common::{Lcg, hash_for_test};
use wartui_proto::pending::Refused;

/// The per-entry rules, written out here rather than called from the crate, so the model
/// test fails when either kind's rules regress.
trait Model: Copy {
    fn address(&self) -> [u8; 6];
    fn is_usable(&self) -> bool;
    fn merge(&mut self, repeat: &Self);
}

fn due<T>(_: &T) -> bool {
    true
}

fn not_due<T>(_: &T) -> bool {
    false
}

fn drain<T>(take: impl FnMut() -> Option<T>) -> Vec<T> {
    std::iter::from_fn(take).collect()
}

/// `Pending`'s rules over a linear search, the model the hashed buffer is checked against.
/// It counts drops with the crate's `Refused`, because what is under test is finding the
/// address. The entry rules come from [`Model`], so a regression in either kind's rules
/// fails here.
struct NaivePending<T: Model> {
    items: Vec<T>,
    cap: usize,
    taken: usize,
    refused: Refused,
}

impl<T: Model> NaivePending<T> {
    fn new(cap: usize) -> Self {
        Self { items: Vec::new(), cap, taken: 0, refused: Refused::new() }
    }

    fn record(&mut self, entry: T, due: bool) {
        if !entry.is_usable() {
            return;
        }
        if let Some(held) = self.items.iter_mut().find(|e| e.address() == entry.address()) {
            held.merge(&entry);
            return;
        }
        if !due {
            return;
        }
        if self.items.len() == self.cap {
            self.refused.note(&entry.address());
            return;
        }
        self.items.push(entry);
    }

    fn take(&mut self) -> Option<T> {
        let entry = self.items.get(self.taken).copied();
        self.taken += usize::from(entry.is_some());
        entry
    }

    fn clear(&mut self) {
        self.items.clear();
        self.taken = 0;
        self.refused.reset();
    }
}

/// The buffer one dwell's Wi-Fi sightings wait in.
///
/// Sightings are parsed from assembled beacons rather than built directly, since
/// `Sighting` has no public constructor and the parser is what the node uses.
mod wifi_pending {
    use super::*;
    use wartui_proto::beacon::{Sighting, parse_mgmt};
    use wartui_proto::pending::WifiPending;

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

    /// The first sighting of a BSSID is kept whole.
    impl Model for Sighting {
        fn address(&self) -> [u8; 6] {
            self.bssid
        }

        fn is_usable(&self) -> bool {
            true
        }

        fn merge(&mut self, _repeat: &Self) {}
    }

    #[test]
    fn wifi_pending_keeps_first_sighting_when_bssid_repeats() {
        let mut pending = WifiPending::<4, 8>::new();
        pending.record(sighting(bssid(1), b"first", -80), due);
        pending.record(sighting(bssid(1), b"second", -40), due);

        let held = drain(|| pending.take());
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
        assert_eq!(pending.refused(), 0, "a sighting not due is not turned away");
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
        assert_eq!(pending.refused(), 2, "each address once, however often it beacons");
        assert_eq!(drain(|| pending.take()), [ap(1, -60), ap(2, -60)]);

        pending.clear();
        pending.record(ap(1, -60), due);
        pending.record(ap(2, -60), due);
        pending.record(ap(3, -60), due);
        assert_eq!(
            pending.refused(),
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
        assert_eq!(drain(|| pending.take()), [ap(1, -70)]);
    }

    #[test]
    fn wifi_pending_stops_take_at_len_when_cleared() {
        let mut pending = WifiPending::<4, 8>::new();
        pending.record(ap(1, -60), due);
        pending.record(ap(2, -60), due);
        pending.clear();
        assert_eq!(pending.take(), None, "last dwell's leavings are not handed out");
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
        assert_eq!(drain(|| pending.take()), expected);
    }

    #[test]
    fn wifi_pending_matches_a_naive_model_when_driven_by_a_long_random_sequence() {
        const N: usize = 12;
        const ADDRESSES: u32 = 40;

        let mut pending = WifiPending::<N, 32>::new();
        let mut model = NaivePending::new(N);
        let mut rng = Lcg::new(0x5EED_BEAC);

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
            assert_eq!(pending.refused(), model.refused.total(), "refused at step {step}");
        }
        assert!(model.refused.total() > 50, "the sequence fills the buffer often enough to matter");
        assert_eq!(
            drain(|| pending.take()),
            std::iter::from_fn(|| model.take()).collect::<Vec<_>>()
        );
    }
}

mod ble_pending {
    use super::*;
    use wartui_proto::hci::AdvReport;
    use wartui_proto::pending::BlePending;

    /// A report from the advertiser whose address ends in `n`.
    fn report(n: u8, rssi: i8, mfgr: Option<u16>) -> AdvReport {
        AdvReport { address: [0x11, 0x22, 0x33, 0x44, 0x55, n], rssi, mfgr }
    }

    /// A report with a reading is held; a repeat keeps the strongest reading and the first
    /// identifier.
    impl Model for AdvReport {
        fn address(&self) -> [u8; 6] {
            self.address
        }

        fn is_usable(&self) -> bool {
            self.rssi != 127
        }

        fn merge(&mut self, repeat: &Self) {
            self.rssi = self.rssi.max(repeat.rssi);
            self.mfgr = self.mfgr.or(repeat.mfgr);
        }
    }

    #[test]
    fn ble_pending_skips_report_when_not_due() {
        let mut pending = BlePending::<4, 8>::new();
        pending.record(report(1, -60, None), not_due);
        assert!(pending.is_empty());
        assert_eq!(pending.refused(), 0, "a report not due is not a report lost");
        assert_eq!(pending.take(), None);
    }

    #[test]
    fn ble_pending_merges_mfgr_when_later_report_not_due() {
        // The identifier often follows the first hearing in a later packet whose
        // RSSI is no better; the address already holds a slot, so it merges
        // without asking whether it is due.
        let mut pending = BlePending::<4, 8>::new();
        pending.record(report(1, -60, None), due);
        pending
            .record(report(1, -70, Some(0x004C)), |_| panic!("a held address is not re-checked"));
        assert_eq!(drain(|| pending.take()), [report(1, -60, Some(0x004C))]);
    }

    #[test]
    fn ble_pending_keeps_strongest_rssi_when_address_repeats() {
        let mut pending = BlePending::<4, 8>::new();
        pending.record(report(1, -70, Some(0x0006)), due);
        pending.record(report(1, -50, Some(0x004C)), due);
        pending.record(report(1, -80, None), due);
        // The first identifier stays: a later one is not a better reading of it.
        assert_eq!(drain(|| pending.take()), [report(1, -50, Some(0x0006))]);
    }

    #[test]
    fn ble_pending_drops_newest_when_full() {
        let mut pending = BlePending::<2, 4>::new();
        pending.record(report(1, -60, None), due);
        pending.record(report(2, -60, None), due);
        pending.record(report(3, -60, None), not_due);
        assert_eq!(pending.refused(), 0, "only a due address counts against a full buffer");
        pending.record(report(4, -60, None), due);
        assert_eq!(pending.refused(), 1);
        assert_eq!(drain(|| pending.take()), [report(1, -60, None), report(2, -60, None)]);
    }

    #[test]
    fn ble_pending_counts_advertiser_once_when_repeated_while_full() {
        // The counter is advertisers, not packets: one address with no room
        // counts once however often it repeats within the scan, and again in the next.
        let mut pending = BlePending::<1, 2>::new();
        pending.record(report(1, -60, None), due);
        for _ in 0..3 {
            pending.record(report(2, -60, None), due);
        }
        assert_eq!(pending.refused(), 1);
        assert_eq!(pending.len(), 1);

        pending.clear();
        pending.record(report(1, -60, None), due);
        pending.record(report(2, -60, None), due);
        assert_eq!(pending.refused(), 2, "a new scan counts it again");
    }

    #[test]
    fn ble_pending_ignores_report_when_rssi_unavailable() {
        let mut pending = BlePending::<4, 8>::new();
        pending.record(report(1, 127, Some(0x004C)), |_| panic!("no reading is never considered"));
        assert!(pending.is_empty());
        assert_eq!(pending.refused(), 0);
    }

    #[test]
    fn ble_pending_admits_new_address_when_full_of_not_due_traffic() {
        // A crowded room of advertisers the host already has, heard on every scan,
        // outnumbering the buffer: the new ones must still land.
        let mut pending = BlePending::<8, 16>::new();
        let fresh = 100..105;
        let is_fresh = |r: &AdvReport| fresh.contains(&r.address[5]);
        for n in (0..20).chain(fresh.clone()) {
            pending.record(report(n, -60, None), is_fresh);
        }
        let landed: Vec<u8> = drain(|| pending.take()).iter().map(|r| r.address[5]).collect();
        assert_eq!(landed, fresh.collect::<Vec<_>>());
        assert_eq!(pending.refused(), 0);
    }

    #[test]
    fn ble_pending_stops_take_at_len_when_cleared() {
        let mut pending = BlePending::<4, 8>::new();
        pending.record(report(1, -60, None), due);
        pending.record(report(2, -60, None), due);
        pending.record(report(3, -60, None), due);
        assert_eq!(drain(|| pending.take()).len(), 3);

        // A fresh scan hearing fewer advertisers than the last leaves stale slots
        // behind `len`, and the zero fill beyond those; neither is ever yielded.
        pending.clear();
        pending.record(report(9, -40, None), due);
        assert_eq!(drain(|| pending.take()), [report(9, -40, None)]);
        assert_eq!(pending.take(), None);

        let mut untouched = BlePending::<4, 8>::new();
        assert_eq!(untouched.take(), None, "the zero fill is not an advertiser");
    }

    #[test]
    fn ble_pending_treats_address_as_new_when_recorded_after_clear() {
        // The index is reset with the buffer: a stale slot would find the address in
        // last scan's leavings and merge into a report past `len`, never asking `due`.
        let mut pending = BlePending::<4, 8>::new();
        pending.record(report(1, -60, None), due);
        pending.record(report(2, -60, None), due);
        pending.clear();

        pending.record(report(2, -50, Some(0x004C)), not_due);
        assert!(pending.is_empty(), "a new scan asks `due` again");

        let mut asked = false;
        pending.record(report(1, -70, None), |_| {
            asked = true;
            true
        });
        assert!(asked);
        assert_eq!(drain(|| pending.take()), [report(1, -70, None)]);
    }

    #[test]
    fn ble_pending_merges_each_address_when_addresses_share_a_hash_slot() {
        let home = |n: u8| hash_for_test(&report(n, 0, None).address, 3);
        let colliding: Vec<u8> =
            (0..=u8::MAX).filter(|&n| home(n) == home(0)).take(4).collect::<Vec<_>>();
        assert_eq!(colliding.len(), 4, "four addresses share a slot of eight");

        let mut pending = BlePending::<4, 8>::new();
        for &n in &colliding {
            pending.record(report(n, -80, None), due);
        }
        // Merged in reverse so the lookups walk the probe run from its far end too.
        for &n in colliding.iter().rev() {
            pending.record(report(n, -40, Some(u16::from(n))), |_| panic!("{n} is held"));
        }
        let expected: Vec<AdvReport> =
            colliding.iter().map(|&n| report(n, -40, Some(u16::from(n)))).collect();
        assert_eq!(drain(|| pending.take()), expected);
    }

    #[test]
    fn ble_pending_matches_a_naive_model_when_driven_by_a_long_random_sequence() {
        const N: usize = 12;
        const ADDRESSES: u32 = 40;

        let mut pending = BlePending::<N, 32>::new();
        let mut model = NaivePending::new(N);
        let mut rng = Lcg::new(0xB1E5_EED5);

        for step in 0..5_000 {
            match rng.next_u32() % 40 {
                0 => {
                    pending.clear();
                    model.clear();
                }
                1..=4 => assert_eq!(pending.take(), model.take(), "take at step {step}"),
                _ => {
                    let n = u8::try_from(rng.next_u32() % ADDRESSES).expect("fits");
                    let rssi = if rng.next_u32().is_multiple_of(8) {
                        127
                    } else {
                        i8::try_from(rng.next_u32() % 60).expect("fits") - 100
                    };
                    let mfgr = rng
                        .next_u32()
                        .is_multiple_of(3)
                        .then(|| u16::try_from(rng.next_u32() % 4).expect("fits"));
                    let is_due = !rng.next_u32().is_multiple_of(4);
                    let report = report(n, rssi, mfgr);
                    pending.record(report, |_| is_due);
                    model.record(report, is_due);
                }
            }
            assert_eq!(pending.len(), model.items.len(), "len at step {step}");
            assert_eq!(pending.refused(), model.refused.total(), "refused at step {step}");
        }
        assert!(model.refused.total() > 50, "the sequence fills the buffer often enough to matter");
        assert_eq!(
            drain(|| pending.take()),
            std::iter::from_fn(|| model.take()).collect::<Vec<_>>()
        );
    }
}

mod refused {
    use wartui_proto::pending::Refused;

    fn mac(n: u16) -> [u8; 6] {
        let [hi, lo] = n.to_be_bytes();
        [0x02, 0, 0, 0, hi, lo]
    }

    #[test]
    fn refused_counts_address_once_when_repeated_within_dwell() {
        let mut refused = Refused::new();
        for _ in 0..10 {
            refused.note(&mac(1));
        }
        assert_eq!(refused.total(), 1, "ten beacons from one access point are one address");
    }

    #[test]
    fn refused_counts_again_when_reset_between_dwells() {
        let mut refused = Refused::new();
        refused.note(&mac(1));
        refused.reset();
        refused.note(&mac(1));
        refused.note(&mac(1));
        assert_eq!(
            refused.total(),
            2,
            "the total carries across a reset and the address counts anew"
        );
    }

    #[test]
    fn refused_counts_distinct_addresses_when_many_refused() {
        // Addresses that share a bit in one dwell count once, so a dense dwell may
        // undercount by a few, never overcount.
        let mut refused = Refused::new();
        for n in 0..50u16 {
            let x = n.wrapping_mul(0x9E37) ^ 0x5A5A;
            let [a, b] = x.to_be_bytes();
            refused.note(&[0x3C, a, 0x71, b, 0x08, 0xC4]);
        }
        let total = refused.total();
        assert!((47..=50).contains(&total), "50 distinct addresses counted as {total}");
    }
}
