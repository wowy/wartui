//! The bridge's own counters over the run, and the plan it last reported.
//!
//! A `Status` counts since the P4 booted, so the run reads each counter as the last `Status` less
//! the first. `main` asks for one right after attaching and one just before the end, so the pair
//! spans the run however the `--rtt` asks fall. A delta that goes negative is a P4 or C6 restart
//! between them; `uptime_ms` shows which. Queue high-water marks are maxima since boot, not
//! counts, and their delta is how far the run pushed them.

use serde::Serialize;
use wartui_proto::bus::NodeBoard;
use wartui_proto::fleet::Report;
use wartui_proto::link::BridgeToHost;

use crate::attachment::AttachmentSummary;

/// A `Status`'s node bus, radio link, drop and queue counters, by name, in the order printed.
pub type Counters = Vec<(String, u32)>;

/// The counters a `Status` carries, or `None` for any other message.
#[must_use]
pub fn counters(msg: &BridgeToHost) -> Option<Counters> {
    let BridgeToHost::Status {
        rx_count,
        dropped_tx,
        uptime_ms,
        rx_oversize,
        queues,
        radio_link: r,
        host_uart_errors,
        ..
    } = msg
    else {
        return None;
    };
    let mut out: Counters = [
        ("uptime_ms", *uptime_ms),
        ("rx_count", *rx_count),
        ("rx_oversize", *rx_oversize),
        ("dropped_tx", *dropped_tx),
        ("host_uart_errors", *host_uart_errors),
        ("radio_link.packets", r.packets),
        ("radio_link.crc_fail", r.crc_fail),
        ("radio_link.bad_magic", r.bad_magic),
        ("radio_link.seq_gap", r.seq_gap),
        ("radio_link.uart_errors", r.uart_errors),
        ("radio_link.radio_crc_fail", r.radio_crc_fail),
        ("radio_link.radio_bad_magic", r.radio_bad_magic),
        ("radio_link.radio_seq_gap", r.radio_seq_gap),
        ("radio_link.radio_uart_errors", r.radio_uart_errors),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect();
    for q in queues {
        let name = q.queue.name();
        out.push((format!("queue.{name}.dropped"), q.dropped));
        out.push((format!("queue.{name}.high_water"), q.high_water));
    }
    Some(out)
}

/// One counter across the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterDelta {
    /// What it counts.
    pub name: String,
    /// In the first `Status`, or `None` when that one did not carry it.
    pub first: Option<u32>,
    /// In the last.
    pub last: u32,
    /// `last - first`.
    pub delta: Option<i64>,
}

impl CounterDelta {
    /// `name first -> last (+delta)`.
    #[must_use]
    pub fn text(&self) -> String {
        let first = self.first.map_or_else(|| "?".to_owned(), |n| n.to_string());
        let delta = self.delta.map_or_else(|| "?".to_owned(), |d| format!("{d:+}"));
        format!("{} {first} -> {} ({delta})", self.name, self.last)
    }
}

/// Every counter in `last`, set against `first`.
#[must_use]
pub fn deltas(first: &Counters, last: &Counters) -> Vec<CounterDelta> {
    last.iter()
        .map(|(name, last)| {
            let first = first.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
            CounterDelta {
                name: name.clone(),
                first,
                last: *last,
                delta: first.map(|first| i64::from(*last) - i64::from(first)),
            }
        })
        .collect()
}

/// The run's counters: the first attachment's first `Status` against the last one's last, or
/// `None` without a `Status`.
#[must_use]
pub fn over_run(attachments: &[AttachmentSummary]) -> Option<Vec<CounterDelta>> {
    let first = attachments.iter().find_map(|a| a.first_status.as_ref())?;
    let last = attachments.iter().rev().find_map(|a| a.last_status.as_ref())?;
    Some(deltas(first, last))
}

/// One member's share, as the last `Fleet` report gave it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetMember {
    /// The slot's label.
    pub slot: String,
    /// The epoch its assignment carries.
    pub epoch: u8,
    /// It scans Bluetooth.
    pub ble: bool,
    /// Channels it holds.
    pub channels: u32,
}

/// The last `Fleet` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FleetSummary {
    /// The scan mode in force.
    pub mode: String,
    /// Every member holding an assignment.
    pub members: Vec<FleetMember>,
    /// Channels of the pool no member can take.
    pub unreachable: u32,
}

impl FleetSummary {
    /// `report`, with slots labeled for `board`.
    #[must_use]
    pub fn of(report: &Report, board: NodeBoard) -> Self {
        Self {
            mode: format!("{:?}", report.mode),
            members: report
                .shares
                .iter()
                .map(|share| FleetMember {
                    slot: board.label(share.slot).to_string(),
                    epoch: share.epoch,
                    ble: share.ble,
                    channels: share.channels.len(),
                })
                .collect(),
            unreachable: report.unreachable.len(),
        }
    }

    /// One line: the mode and each member.
    #[must_use]
    pub fn text(&self) -> String {
        let members: Vec<String> = self
            .members
            .iter()
            .map(|m| {
                let job = if m.ble { "ble".to_owned() } else { format!("{} ch", m.channels) };
                format!("{} e{} {job}", m.slot, m.epoch)
            })
            .collect();
        format!(
            "{}, {} members: {}; {} unreachable",
            self.mode,
            self.members.len(),
            members.join(", "),
            self.unreachable,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::link::{QueueId, QueueStat, QueueStats, RadioLinkStats};

    fn status(rx_count: u32, seq_gap: u32, bulk_dropped: Option<u32>) -> BridgeToHost {
        let mut queues = QueueStats::new();
        if let Some(dropped) = bulk_dropped {
            queues
                .push(QueueStat {
                    queue: QueueId::HostBulk,
                    capacity: 4096,
                    high_water: 100,
                    dropped,
                })
                .unwrap();
        }
        BridgeToHost::Status {
            rx_count,
            dropped_tx: 0,
            uptime_ms: 0,
            rx_oversize: 0,
            queues,
            radio_link: RadioLinkStats { seq_gap, ..Default::default() },
            host_uart_errors: 0,
            fan: None,
            pool: Default::default(),
            scan: Default::default(),
            ble: None,
            present: 0b11,
            members: Default::default(),
        }
    }

    fn find<'a>(deltas: &'a [CounterDelta], name: &str) -> &'a CounterDelta {
        deltas.iter().find(|d| d.name == name).unwrap()
    }

    #[test]
    fn bridge_counters_skip_message_when_not_status() {
        assert_eq!(counters(&BridgeToHost::Log { message: "x".try_into().unwrap() }), None);
    }

    #[test]
    fn bridge_deltas_subtract_first_when_both_carry_counter() {
        let first = counters(&status(100, 2, Some(5))).unwrap();
        let last = counters(&status(350, 2, Some(9))).unwrap();
        let deltas = deltas(&first, &last);
        assert_eq!(find(&deltas, "rx_count").delta, Some(250));
        assert_eq!(find(&deltas, "radio_link.seq_gap").delta, Some(0));
        let bulk = find(&deltas, "queue.host-bulk.dropped");
        assert_eq!(bulk.text(), "queue.host-bulk.dropped 5 -> 9 (+4)");
    }

    #[test]
    fn bridge_deltas_leave_first_unknown_when_queue_appears_later() {
        let first = counters(&status(1, 0, None)).unwrap();
        let last = counters(&status(2, 0, Some(3))).unwrap();
        let deltas = deltas(&first, &last);
        let bulk = find(&deltas, "queue.host-bulk.dropped");
        assert_eq!((bulk.first, bulk.delta), (None, None));
        assert_eq!(bulk.text(), "queue.host-bulk.dropped ? -> 3 (?)");
    }

    #[test]
    fn bridge_deltas_go_negative_when_counter_restarts() {
        let first = counters(&status(900, 0, None)).unwrap();
        let last = counters(&status(40, 0, None)).unwrap();
        assert_eq!(find(&deltas(&first, &last), "rx_count").text(), "rx_count 900 -> 40 (-860)");
    }
}
