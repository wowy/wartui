//! Node sighting batches seen over the link, per slot, and the batches lost between.
//!
//! The rule is `wartui-core`'s (`engine/rx.rs`, `note_batch_seq`, `is_duplicate_batch` and the
//! restart arm of `on_heartbeat`), so a run here reads loss the way a capture's `batches_lost`
//! does. A batch's `seq` counts up once per batch the P4 acknowledged, wraps, and restarts at boot:
//!
//! - a gap under [`UNCOUNTED_GAP`] is that many batches lost; one at or past it is a wrap or a
//!   reordering, and goes uncounted;
//! - a same-`seq` batch with the same bytes inside [`DUPLICATE_WINDOW_US`] is the bus resending one
//!   whose ack the node missed, and is no loss;
//! - a same-`seq` batch with new bytes is a send the node gave up on though it arrived, and the
//!   node reuses that `seq`: no loss;
//! - a heartbeat whose counter fell, or that reports epoch 0 while the slot held the share the
//!   last `Fleet` report gives it, is a restart, and drops the baseline.
//!
//! Two departures, both because the stand-in reads one attachment rather than a fleet. It keys by
//! slot rather than MAC, so a heartbeat naming a new MAC drops the slot's baseline too. It has no
//! lag model, so a gap after a batch the P4's host queue held is counted rather than left to
//! `dropped_tx`; the run prints `dropped_tx`'s delta beside it.

use std::collections::BTreeMap;

use serde::Serialize;
use wartui_proto::bus::{ACK_TIMEOUT_MS, Heartbeat, NodeBoard, SightingBatch, Slot};
use wartui_proto::mac::Mac;

/// The smallest `seq` gap not counted as loss, as `wartui-core` reads it.
pub const UNCOUNTED_GAP: u16 = 1024;
/// How long after a batch a byte-identical same-`seq` repeat is a copy of it: two ack timeouts,
/// compared with a strict `<`.
pub const DUPLICATE_WINDOW_US: u64 = 2 * ACK_TIMEOUT_MS * 1_000;

/// One slot's sighting batches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SlotBatches {
    /// Batches that decoded and were not copies.
    pub batches: u64,
    /// Batches lost, from `seq` gaps.
    pub lost: u64,
    /// Gaps those were lost in.
    pub gaps: u64,
    /// Copies of the batch before, dropped.
    pub duplicates: u64,
    /// Same-`seq` batches with new bytes: sends the node gave up on.
    pub reused_seq: u64,
    /// Gaps at or past [`UNCOUNTED_GAP`], read as a wrap or reordering.
    pub uncounted: u64,
    /// Sighting records that did not decode.
    pub undecodable: u64,
    /// Baselines dropped for a node restart.
    pub restarts: u64,
    #[serde(skip)]
    last_seq: Option<u16>,
    #[serde(skip)]
    last_batch: Option<(Vec<u8>, u32)>,
    #[serde(skip)]
    mac: Option<Mac>,
    #[serde(skip)]
    counter: Option<u32>,
    #[serde(skip)]
    held_epoch: Option<u8>,
}

impl SlotBatches {
    fn forget(&mut self) {
        self.last_seq = None;
        self.last_batch = None;
        self.held_epoch = None;
    }
}

/// Sighting batches per slot.
#[derive(Debug, Clone, Default)]
pub struct Batches {
    slots: BTreeMap<Slot, SlotBatches>,
}

impl Batches {
    /// A heartbeat from `slot`; `share_epoch` is the epoch the last `Fleet` report gives the slot.
    pub fn heartbeat(&mut self, slot: Slot, heartbeat: &Heartbeat, share_epoch: Option<u8>) {
        let entry = self.slots.entry(slot).or_default();
        if entry.mac.is_some_and(|mac| mac != heartbeat.mac) {
            // Another node: its `seq` is its own.
            entry.forget();
            entry.counter = None;
        }
        let counter_fell = entry.counter.is_some_and(|last| heartbeat.counter < last);
        let epoch_reset = heartbeat.epoch == 0
            && share_epoch.is_some()
            && entry.held_epoch.is_some()
            && entry.held_epoch == share_epoch;
        if counter_fell || epoch_reset {
            entry.restarts += 1;
            entry.forget();
        }
        entry.mac = Some(heartbeat.mac);
        entry.counter = Some(heartbeat.counter);
        entry.held_epoch = Some(heartbeat.epoch);
    }

    /// One sighting record's body, from `slot`, stamped `rx_us` by the P4.
    pub fn take(&mut self, slot: Slot, rx_us: u32, body: &[u8]) {
        let entry = self.slots.entry(slot).or_default();
        let Ok(batch) = SightingBatch::decode(body) else {
            entry.undecodable += 1;
            return;
        };
        let copy = entry.last_seq == Some(batch.seq)
            && entry.last_batch.as_ref().is_some_and(|(bytes, at)| {
                bytes.as_slice() == body && u64::from(rx_us.wrapping_sub(*at)) < DUPLICATE_WINDOW_US
            });
        if copy {
            entry.duplicates += 1;
        } else {
            entry.batches += 1;
            if let Some(last) = entry.last_seq {
                let gap = batch.seq.wrapping_sub(last.wrapping_add(1));
                if gap == u16::MAX {
                    entry.reused_seq += 1;
                } else if gap >= UNCOUNTED_GAP {
                    entry.uncounted += 1;
                } else if gap > 0 {
                    entry.lost += u64::from(gap);
                    entry.gaps += 1;
                }
            }
            entry.last_seq = Some(batch.seq);
        }
        entry.last_batch = Some((body.to_vec(), rx_us));
    }

    /// One slot's tally.
    #[must_use]
    pub fn slot(&self, slot: Slot) -> Option<&SlotBatches> {
        self.slots.get(&slot)
    }

    /// Every slot that sent a batch, under the label `board` prints for it.
    #[must_use]
    pub fn labeled(&self, board: NodeBoard) -> BTreeMap<String, SlotBatches> {
        self.slots
            .iter()
            .filter(|(_, b)| b.batches + b.duplicates + b.undecodable > 0)
            .map(|(slot, batches)| (board.label(*slot).to_string(), batches.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::bus::{
        Board, Capabilities, DieTemp, RecordKind, Security, SightingBatchWriter, SightingMsg, Vdd,
    };
    use wartui_proto::reset::ResetCause;

    const MAC: Mac = [1, 2, 3, 4, 5, 6];

    fn slot() -> Slot {
        Slot::new(0, 1).unwrap()
    }

    /// A one-sighting batch body carrying `seq`, its BSSID's last byte `tag`.
    fn batch(seq: u16, tag: u8) -> Vec<u8> {
        let mut writer = SightingBatchWriter::new(seq);
        let sighting = SightingMsg {
            kind: RecordKind::Wifi,
            bssid: [0xAA, 0, 0, 0, 0, tag],
            channel: 6,
            rssi: -60,
            security: Security::from_u8(0),
            ssid: b"net",
            ext: &[],
        };
        assert!(writer.push(&sighting));
        writer.as_bytes().to_vec()
    }

    fn beat(mac: Mac, counter: u32, epoch: u8) -> Heartbeat {
        let body = Heartbeat {
            capabilities: Capabilities::here(Board::Esp32c5),
            mac,
            counter,
            epoch,
            wifi_refused: 0,
            ble_refused: 0,
            reset_cause: ResetCause::PowerOn,
            vdd: Vdd::NONE,
            die_temp: DieTemp::NONE,
            heap_free: 0,
        }
        .encode();
        // Through the decoder, as the stand-in reads one.
        Heartbeat::decode(&body).unwrap()
    }

    fn fed(seqs: impl IntoIterator<Item = u16>) -> Batches {
        let mut batches = Batches::default();
        for (i, seq) in seqs.into_iter().enumerate() {
            batches.take(slot(), i as u32 * 3_000_000, &batch(seq, i as u8));
        }
        batches
    }

    #[test]
    fn batches_counts_none_lost_when_seq_climbs_by_one() {
        let batches = fed(10..20);
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.batches, tally.lost, tally.gaps), (10, 0, 0));
    }

    #[test]
    fn batches_counts_lost_when_seq_skips() {
        let batches = fed([1, 2, 5, 6, 8]);
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.batches, tally.lost, tally.gaps), (5, 3, 2));
    }

    #[test]
    fn batches_counts_lost_when_seq_wraps() {
        let batches = fed([u16::MAX - 1, u16::MAX, 1]);
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.lost, tally.uncounted), (1, 0));
    }

    #[test]
    fn batches_leaves_gap_uncounted_when_too_large() {
        let batches = fed([5, 5 + UNCOUNTED_GAP + 1]);
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.lost, tally.uncounted), (0, 1));
    }

    #[test]
    fn batches_drops_copy_when_resent_inside_window() {
        let mut batches = Batches::default();
        let body = batch(7, 1);
        batches.take(slot(), 0, &body);
        batches.take(slot(), 1_500_000, &body);
        batches.take(slot(), 2_000_000, &batch(8, 2));
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.batches, tally.duplicates, tally.lost), (2, 1, 0));
    }

    #[test]
    fn batches_counts_reused_seq_when_same_seq_carries_new_bytes() {
        let mut batches = Batches::default();
        batches.take(slot(), 0, &batch(7, 1));
        batches.take(slot(), 100_000, &batch(7, 2));
        batches.take(slot(), 200_000, &batch(8, 3));
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.batches, tally.reused_seq, tally.lost, tally.duplicates), (3, 1, 0, 0));
    }

    #[test]
    fn batches_records_repeat_when_outside_window() {
        let mut batches = Batches::default();
        let body = batch(7, 1);
        batches.take(slot(), 0, &body);
        batches.take(slot(), DUPLICATE_WINDOW_US as u32, &body);
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.batches, tally.duplicates, tally.reused_seq), (2, 0, 1));
    }

    #[test]
    fn batches_drops_baseline_when_heartbeat_counter_falls() {
        let mut batches = Batches::default();
        batches.heartbeat(slot(), &beat(MAC, 500, 3), Some(3));
        batches.take(slot(), 0, &batch(400, 1));
        batches.heartbeat(slot(), &beat(MAC, 2, 0), Some(3));
        batches.take(slot(), 5_000_000, &batch(0, 2));
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.restarts, tally.lost, tally.uncounted), (1, 0, 0));
    }

    #[test]
    fn batches_drops_baseline_when_epoch_resets_under_current_share() {
        let mut batches = Batches::default();
        batches.heartbeat(slot(), &beat(MAC, 10, 4), Some(4));
        batches.take(slot(), 0, &batch(30, 1));
        // The counter climbed past the old one, so only the epoch shows the restart.
        batches.heartbeat(slot(), &beat(MAC, 40, 0), Some(4));
        batches.take(slot(), 5_000_000, &batch(2, 2));
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.restarts, tally.lost), (1, 0));
    }

    #[test]
    fn batches_keeps_baseline_when_epoch_zero_after_stop() {
        let mut batches = Batches::default();
        batches.heartbeat(slot(), &beat(MAC, 10, 4), Some(4));
        batches.take(slot(), 0, &batch(30, 1));
        // Stopped: the report gives the slot no share, so its 0 is the stop.
        batches.heartbeat(slot(), &beat(MAC, 11, 0), None);
        batches.take(slot(), 5_000_000, &batch(32, 2));
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.restarts, tally.lost), (0, 1));
    }

    #[test]
    fn batches_drops_baseline_when_slot_names_new_mac() {
        let mut batches = Batches::default();
        batches.heartbeat(slot(), &beat(MAC, 10, 1), Some(1));
        batches.take(slot(), 0, &batch(30, 1));
        batches.heartbeat(slot(), &beat([9; 6], 900, 1), Some(1));
        batches.take(slot(), 5_000_000, &batch(500, 2));
        let tally = batches.slot(slot()).unwrap();
        assert_eq!((tally.restarts, tally.lost, tally.uncounted), (0, 0, 0));
    }

    #[test]
    fn batches_counts_undecodable_when_body_is_empty() {
        let mut batches = Batches::default();
        batches.take(slot(), 0, &[7, 0, 0]);
        assert_eq!(batches.slot(slot()).unwrap().undecodable, 1);
    }
}
