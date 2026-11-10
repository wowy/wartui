//! The run's events, each stamped as the stand-in saw it, so a heartbeat gap, a lost batch and a
//! rise in a link error counter can be set against each other in time.
//!
//! The wall clock and the seconds since attach are the stand-in's, so they include the Bluetooth
//! delay. `rx_us` is the P4's stamp on the bus transfer, where the record carries one, and orders
//! node events on the bridge's own clock. A `SpikeStats` counter's rise lands up to one stats
//! period (`STATS_EVERY_MS`) after it happened, and a rise is read only between two `SpikeStats`
//! of one attachment, so a counter that falls, at a restart, raises nothing.

use std::fmt::Display;

use chrono::{DateTime, TimeZone};
use serde::Serialize;
use wartui_proto::spike::SpikeStats;

use crate::attachment::SeqGap;
use crate::bridge::FleetSummary;

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// Two heartbeats from a slot spaced past one and a half assigned beats.
    BeatGap {
        /// The slot's label.
        slot: String,
        /// The spacing, in milliseconds.
        spacing_ms: u32,
        /// Beats inferred missing between.
        missed: u64,
    },
    /// A slot's sighting batch `seq` skipped.
    BatchGap {
        /// The slot's label.
        slot: String,
        /// The `seq` before the gap.
        last_seq: u16,
        /// The `seq` after it.
        seq: u16,
        /// Batches lost between.
        lost: u16,
    },
    /// A `SpikeStats` error or drop counter rose since the attachment's previous `SpikeStats`.
    CounterRise {
        /// The counter, as `SpikeStats` names it.
        name: &'static str,
        /// How far it rose.
        delta: u32,
    },
    /// `SpikeLoad` seqs that never arrived.
    LoadGap(SeqGap),
    /// A `Fleet` report.
    Fleet(FleetSummary),
    /// A bridge `Log`.
    Log {
        /// Its text.
        message: String,
    },
    /// A bridge `Error`.
    Error {
        /// Its text.
        message: String,
    },
}

impl EventKind {
    /// The event as the line prints it, without its stamp.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::BeatGap { slot, spacing_ms, missed } => format!(
                "{slot} heartbeat spacing {:.1} s, ~{missed} missed (inferred from timing)",
                f64::from(*spacing_ms) / 1000.0
            ),
            Self::BatchGap { slot, last_seq, seq, lost } => {
                format!("{slot} sightings seq {last_seq} -> {seq}, {lost} lost")
            }
            Self::CounterRise { name, delta } => format!("spike stats {name} +{delta}"),
            Self::LoadGap(gap) if gap.from == gap.to => format!("load seq {} missing", gap.from),
            Self::LoadGap(gap) => format!(
                "load seqs {}-{} missing ({})",
                gap.from,
                gap.to,
                u64::from(gap.to - gap.from) + 1
            ),
            Self::Fleet(report) => format!("fleet {}", report.text()),
            Self::Log { message } => format!("log: {message}"),
            Self::Error { message } => format!("error: {message}"),
        }
    }
}

/// One event, stamped.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// Local wall-clock time, `HH:MM:SS.mmm`.
    pub wall: String,
    /// The attachment it happened in, from 1.
    pub attachment: u32,
    /// Seconds since that attachment started.
    pub secs: f64,
    /// The P4's `rx_us` on the record, where it carries one.
    pub rx_us: Option<u32>,
    /// What happened.
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    /// The line printed as it happens: `# event HH:MM:SS.mmm #N +S.SSS s[ rx_us U]: text`.
    #[must_use]
    pub fn line(&self) -> String {
        let rx = self.rx_us.map_or_else(String::new, |rx| format!(" rx_us {rx}"));
        format!(
            "# event {} #{} +{:.3} s{rx}: {}",
            self.wall,
            self.attachment,
            self.secs,
            self.kind.text()
        )
    }
}

/// `time` as `HH:MM:SS.mmm`.
#[must_use]
pub fn clock<Tz: TimeZone>(time: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    time.format("%H:%M:%S%.3f").to_string()
}

/// Each error or drop counter that rose from `prev` to `next`, with how far.
#[must_use]
pub fn rises(prev: &SpikeStats, next: &SpikeStats) -> Vec<EventKind> {
    let watched = |s: &SpikeStats| {
        let r = &s.radio_link;
        [
            ("radio_link.crc_fail", r.crc_fail),
            ("radio_link.bad_magic", r.bad_magic),
            ("radio_link.seq_gap", r.seq_gap),
            ("radio_link.uart_errors", r.uart_errors),
            ("radio_link.radio_crc_fail", r.radio_crc_fail),
            ("radio_link.radio_bad_magic", r.radio_bad_magic),
            ("radio_link.radio_seq_gap", r.radio_seq_gap),
            ("radio_link.radio_uart_errors", r.radio_uart_errors),
            ("evicted", s.evicted),
            ("refused", s.refused),
            ("ble.dropped_unsubscribed", s.ble.dropped_unsubscribed),
            ("ble.dropped_overflow", s.ble.dropped_overflow),
            ("ble.dropped_writes", s.ble.dropped_writes),
        ]
    };
    watched(prev)
        .into_iter()
        .zip(watched(next))
        .filter(|((_, before), (_, after))| after > before)
        .map(|((name, before), (_, after))| EventKind::CounterRise { name, delta: after - before })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use wartui_proto::link::RadioLinkStats;

    fn event(rx_us: Option<u32>, kind: EventKind) -> Event {
        Event { wall: "14:02:11.482".to_owned(), attachment: 1, secs: 123.4567, rx_us, kind }
    }

    #[test]
    fn events_clock_prints_milliseconds_when_given_local_time() {
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let time = zone.with_ymd_and_hms(2026, 11, 10, 9, 5, 7).unwrap()
            + chrono::Duration::milliseconds(42);
        assert_eq!(clock(&time), "09:05:07.042");
    }

    #[test]
    fn events_line_carries_rx_us_when_record_has_one() {
        let kind = EventKind::BeatGap { slot: "N3".to_owned(), spacing_ms: 20_140, missed: 3 };
        assert_eq!(
            event(Some(81_234_567), kind).line(),
            "# event 14:02:11.482 #1 +123.457 s rx_us 81234567: N3 heartbeat spacing 20.1 s, \
             ~3 missed (inferred from timing)"
        );
    }

    #[test]
    fn events_line_omits_rx_us_when_record_has_none() {
        let kind = EventKind::Error { message: "bus B stalled".to_owned() };
        assert_eq!(
            event(None, kind).line(),
            "# event 14:02:11.482 #1 +123.457 s: error: bus B stalled"
        );
    }

    #[test]
    fn events_text_names_range_when_load_gap_spans_several() {
        assert_eq!(EventKind::LoadGap(SeqGap { from: 7, to: 7 }).text(), "load seq 7 missing");
        assert_eq!(
            EventKind::LoadGap(SeqGap { from: 120, to: 125 }).text(),
            "load seqs 120-125 missing (6)"
        );
    }

    #[test]
    fn events_json_flattens_kind_when_serialized() {
        let kind = EventKind::BatchGap { slot: "N1".to_owned(), last_seq: 41, seq: 44, lost: 2 };
        let json = serde_json::to_value(event(Some(5), kind)).unwrap();
        assert_eq!(json["kind"], "batch_gap");
        assert_eq!((json["lost"].as_u64(), json["rx_us"].as_u64()), (Some(2), Some(5)));
    }

    #[test]
    fn events_rises_names_each_counter_when_it_climbs() {
        let prev = SpikeStats {
            evicted: 4,
            radio_link: RadioLinkStats { radio_crc_fail: 10, seq_gap: 2, ..Default::default() },
            ..Default::default()
        };
        let mut next = prev;
        next.evicted = 9;
        next.radio_link.radio_crc_fail = 13;
        next.ble.dropped_overflow = 1;
        assert_eq!(
            rises(&prev, &next),
            [
                EventKind::CounterRise { name: "radio_link.radio_crc_fail", delta: 3 },
                EventKind::CounterRise { name: "evicted", delta: 5 },
                EventKind::CounterRise { name: "ble.dropped_overflow", delta: 1 },
            ]
        );
    }

    #[test]
    fn events_rises_ignores_counter_when_it_falls() {
        let prev = SpikeStats { refused: 50, ..Default::default() };
        let next = SpikeStats { refused: 1, ..Default::default() };
        assert!(rises(&prev, &next).is_empty());
    }
}
