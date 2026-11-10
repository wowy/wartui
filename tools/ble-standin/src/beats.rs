//! Node heartbeats seen over the link, per slot.
//!
//! A heartbeat carries no sequence: its `counter` counts sweeps, scans and idle beats, so an
//! assigned node's counter climbs several per beat and its steps say nothing about loss. A lost
//! beat is inferred from timing instead: the P4's `rx_us` stamps the transfer that carried it, so
//! the spacing is the node's and the bus's, unaffected by Bluetooth delay. A counter that falls is
//! a restart, as `wartui-core` reads it.

use std::collections::BTreeMap;

use serde::Serialize;
use wartui_proto::bus::{Heartbeat, NodeBoard, Slot};
use wartui_proto::node::ASSIGNED_BEAT_MS;

/// Spacing past which a beat counts as missed: one and a half assigned beats.
const MISSED_AFTER_US: u32 = ASSIGNED_BEAT_MS * 1000 * 3 / 2;
const BEAT_US: u32 = ASSIGNED_BEAT_MS * 1000;

/// One slot's heartbeats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SlotBeats {
    /// Heartbeats that decoded.
    pub beats: u64,
    /// Heartbeat records that did not.
    pub undecodable: u64,
    /// Times the counter fell.
    pub restarts: u64,
    /// Beats inferred missing from the spacing of `rx_us`.
    pub missed: u64,
    /// The longest spacing between two beats, in milliseconds.
    pub max_spacing_ms: u32,
    #[serde(skip)]
    last: Option<(u32, u32)>,
}

/// Heartbeats per slot.
#[derive(Debug, Clone, Default)]
pub struct Beats {
    slots: BTreeMap<Slot, SlotBeats>,
}

impl Beats {
    /// Take one heartbeat record's body, from `slot`, stamped `rx_us` by the P4.
    pub fn take(&mut self, slot: Slot, rx_us: u32, body: &[u8]) {
        let entry = self.slots.entry(slot).or_default();
        let Ok(heartbeat) = Heartbeat::decode(body) else {
            entry.undecodable += 1;
            return;
        };
        entry.beats += 1;
        if let Some((last_rx, last_counter)) = entry.last {
            let spacing = rx_us.wrapping_sub(last_rx);
            entry.max_spacing_ms = entry.max_spacing_ms.max(spacing / 1000);
            if spacing > MISSED_AFTER_US {
                entry.missed += u64::from((spacing + BEAT_US / 2) / BEAT_US - 1);
            }
            if heartbeat.counter < last_counter {
                entry.restarts += 1;
            }
        }
        entry.last = Some((rx_us, heartbeat.counter));
    }

    /// Whether any heartbeat record arrived.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// One slot's tally.
    #[must_use]
    pub fn slot(&self, slot: Slot) -> Option<&SlotBeats> {
        self.slots.get(&slot)
    }

    /// Every slot's tally under the label `board` prints for it.
    #[must_use]
    pub fn labeled(&self, board: NodeBoard) -> BTreeMap<String, SlotBeats> {
        self.slots
            .iter()
            .map(|(slot, beats)| (board.label(*slot).to_string(), beats.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::bus::{Board, Capabilities, DieTemp, Vdd};
    use wartui_proto::reset::ResetCause;

    fn body(counter: u32) -> [u8; 27] {
        Heartbeat {
            capabilities: Capabilities::here(Board::Esp32c5),
            mac: [1, 2, 3, 4, 5, 6],
            counter,
            epoch: 1,
            wifi_refused: 0,
            ble_refused: 0,
            reset_cause: ResetCause::PowerOn,
            vdd: Vdd::NONE,
            die_temp: DieTemp::NONE,
            heap_free: 0,
        }
        .encode()
    }

    fn slot() -> Slot {
        Slot::new(0, 0).unwrap()
    }

    #[test]
    fn beats_counts_none_missed_when_spaced_by_one_beat() {
        let mut beats = Beats::default();
        for (i, counter) in [10, 16, 22, 29].into_iter().enumerate() {
            beats.take(slot(), i as u32 * 5_100_000, &body(counter));
        }
        let tally = beats.slot(slot()).unwrap();
        assert_eq!((tally.beats, tally.missed, tally.restarts), (4, 0, 0));
        assert_eq!(tally.max_spacing_ms, 5_100);
    }

    #[test]
    fn beats_infers_missed_when_spacing_spans_three_beats() {
        let mut beats = Beats::default();
        beats.take(slot(), 1_000_000, &body(1));
        // 15 s later: two beats between went missing.
        beats.take(slot(), 16_000_000, &body(20));
        assert_eq!(beats.slot(slot()).unwrap().missed, 2);
    }

    #[test]
    fn beats_counts_restart_when_counter_falls() {
        let mut beats = Beats::default();
        beats.take(slot(), 0, &body(500));
        beats.take(slot(), 5_000_000, &body(2));
        assert_eq!(beats.slot(slot()).unwrap().restarts, 1);
    }

    #[test]
    fn beats_measures_spacing_when_rx_clock_wraps() {
        let mut beats = Beats::default();
        beats.take(slot(), u32::MAX - 999_999, &body(1));
        beats.take(slot(), 4_000_000, &body(7));
        let tally = beats.slot(slot()).unwrap();
        assert_eq!((tally.missed, tally.max_spacing_ms), (0, 5_000));
    }

    #[test]
    fn beats_counts_undecodable_when_body_is_short() {
        let mut beats = Beats::default();
        beats.take(slot(), 0, &[0; 11]);
        let tally = beats.slot(slot()).unwrap();
        assert_eq!((tally.beats, tally.undecodable), (0, 1));
    }
}
