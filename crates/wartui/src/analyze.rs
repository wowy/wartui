//! `wartui analyze` — what a capture holds, how fast it arrived, and what it lost on the way in.
//!
//! The report has three parts. The first is what `export` reports, from the same fold run into
//! nothing, so the counts match what an export of the same selection would write. The second is
//! the peak arrival rates and the third the losses, both from `wartui_core::analyze`, whose `//!`
//! says how each figure is counted. Standard output, because nothing else is written there.
//!
//! Nodes go by slot label, as on the main screen, and by address tail with no slot. Two nodes whose
//! last heartbeats name one slot each get their tail after the label, wherever they appear in the
//! report.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use wartui_core::analyze::{
    HostLoss, LossSummary, NodeLoss, Peak, PeakSummary, RadioLinkLoss, losses, peaks,
};
use wartui_core::export::wigle_csv;
use wartui_core::store::open_readonly;
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::mac::{self, Mac};

use crate::export::{
    Selection, details, note_excluded, note_quality, note_unknown_kind, note_unpositioned,
};
use crate::text::{buffer_full, node_name, thousands};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,
}

pub fn run(args: Args) -> Result<()> {
    let (config, _) = args.selection.config()?;
    let db = args.selection.capture("analyzing")?;
    let conn = open_readonly(&db).with_context(|| format!("opening {}", db.display()))?;
    let filter = args.selection.filter(&config);
    let summary = wigle_csv(&conn, &filter, &mut std::io::sink(), env!("CARGO_PKG_VERSION"))?;
    let loss = losses(&conn).context("reading what the capture lost")?;
    let peak = peaks(&conn, &loss.slots()).context("reading how fast sightings arrived")?;
    let names = NodeNames::new(loss.board, &loss);
    print!(
        "{} rows to export\n{}{}{}",
        thousands(summary.rows),
        details(&summary),
        peaks_text(&peak, &names),
        losses_text(&loss, &names)
    );
    note_unpositioned(&summary);
    note_unknown_kind(&summary);
    note_excluded(&summary);
    note_quality(&summary);
    Ok(())
}

/// What the report calls its nodes.
struct NodeNames {
    board: NodeBoard,
    /// Slots more than one node's latest heartbeat names.
    shared: BTreeSet<Slot>,
}

impl NodeNames {
    /// `loss` lists every node with a heartbeat, so every node with a slot.
    fn new(board: NodeBoard, loss: &LossSummary) -> Self {
        let mut seen = BTreeSet::new();
        let shared =
            loss.nodes.iter().filter_map(|node| node.slot).filter(|s| !seen.insert(*s)).collect();
        Self { board, shared }
    }

    fn name(&self, slot: Option<Slot>, mac: &Mac) -> String {
        let name = node_name(self.board, slot, mac);
        if slot.is_some_and(|slot| self.shared.contains(&slot)) {
            format!("{name} {}", mac::short(mac))
        } else {
            name
        }
    }
}

/// `names`, padded to the widest so the figures after them line up.
fn name_column(names: Vec<String>) -> Vec<String> {
    let width = names.iter().map(String::len).max().unwrap_or(0);
    names.into_iter().map(|name| format!("{name:<width$}")).collect()
}

/// The peak arrival rates, in the layout of [`details`], or nothing when no sighting was stored.
fn peaks_text(peak: &PeakSummary, names: &NodeNames) -> String {
    use std::fmt::Write as _;

    if peak.sightings == 0 {
        return String::new();
    }
    let rate = |p: &Peak| format!("{}/s  {}/min", thousands(p.per_second), thousands(p.per_minute));
    let mut text = String::new();
    let _ = write!(
        text,
        "  {:<11}{} sightings/s  {}/min",
        "peak",
        thousands(peak.total.per_second),
        thousands(peak.total.per_minute)
    );
    if peak.clock_reversals > 0 {
        let plural = if peak.clock_reversals == 1 { "" } else { "s" };
        let _ =
            write!(text, "  wall clock reversed {} time{plural}", thousands(peak.clock_reversals));
    }
    text.push('\n');
    for (label, kind) in [("peak wifi", &peak.wifi), ("peak ble", &peak.ble)] {
        if kind.per_minute > 0 {
            let _ = writeln!(text, "  {label:<11}{}", rate(kind));
        }
    }
    let _ = writeln!(text, "  peak by node");
    let column = name_column(peak.nodes.iter().map(|n| names.name(n.slot, &n.mac)).collect());
    for (name, node) in column.iter().zip(&peak.nodes) {
        let _ = writeln!(text, "    {name}  {}", rate(&node.peak));
    }
    text
}

/// The loss lines, in the layout of [`details`]. Heartbeats follow the core's modulo assumptions;
/// `lost on USB` is approximate to the bridge's queued frames.
fn losses_text(loss: &LossSummary, names: &NodeNames) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    if let Some(bridge) = &loss.bridge {
        let mut line = format!(
            "  {:<11}{} frames received  {} dropped",
            "bridge",
            thousands(bridge.received),
            thousands(bridge.dropped)
        );
        if bridge.received > 0 {
            let _ = write!(line, " ({})", percent(bridge.dropped, bridge.received));
        }
        if bridge.reboots > 0 {
            let plural = if bridge.reboots == 1 { "" } else { "s" };
            let _ = write!(line, "  {} reboot{plural}", thousands(bridge.reboots));
        }
        if bridge.host_read > 0 {
            let _ = write!(line, "  host read {}", thousands(bridge.host_read));
        }
        if bridge.usb_lost > 0 {
            let _ = write!(line, "  {} lost on USB", thousands(bridge.usb_lost));
        }
        if bridge.oversize > 0 {
            let _ = write!(line, "  {} oversize (not a node's)", thousands(bridge.oversize));
        }
        let _ = writeln!(text, "{line}");
        for queue in &bridge.queues {
            let _ = writeln!(
                text,
                "  {:<11}{}  peak {}/{} bytes  {} dropped",
                "queue",
                queue.queue.name(),
                thousands(queue.peak),
                thousands(queue.capacity),
                thousands(queue.dropped)
            );
        }
        // Both are the P4's, and the host link's errors are said at 0 too: they answer whether
        // the host's frames reached the P4.
        let _ = writeln!(text, "  {:<11}{}", "radio link", radio_link_line(&bridge.radio_link));
        let _ = writeln!(
            text,
            "  {:<11}{} host_uart_errors",
            "host link",
            thousands(bridge.host_uart_errors)
        );
    }
    if let Some(host) = &loss.host {
        let _ = writeln!(text, "  {:<11}{}", "host", host_line(host));
        if let Some(health) = health_line(host) {
            let _ = writeln!(text, "  {:<11}{health}", "health");
        }
    }

    if loss.nodes.is_empty() {
        return text;
    }
    let total = |f: fn(&NodeLoss) -> u64| loss.nodes.iter().map(f).sum::<u64>();
    let _ = writeln!(
        text,
        "  {:<11}{} lost between node and host",
        "batches",
        thousands(total(|n| n.batches_lost))
    );
    // `buffer full` fills the label column, so it carries its own space.
    if let Some(figures) = buffer_full(total(|n| n.wifi_refused), total(|n| n.ble_refused)) {
        let _ = writeln!(text, "  {figures}");
    }

    let _ = writeln!(text, "  loss by node");
    let column = name_column(loss.nodes.iter().map(|n| names.name(n.slot, &n.mac)).collect());
    for (name, node) in column.iter().zip(&loss.nodes) {
        let mut line = format!("    {name}  batches {}", thousands(node.batches_lost));
        if node.heartbeats > 0 {
            let _ = write!(line, "  heartbeats {}", thousands(node.heartbeats));
        }
        if let Some(figures) = buffer_full(node.wifi_refused, node.ble_refused) {
            let _ = write!(line, "  {figures}");
        }
        if node.supply_drop {
            line.push_str("  supply drop");
        }
        let _ = writeln!(text, "{line}");
    }
    text
}

/// The UART link's packets, then only the error counters that moved.
fn radio_link_line(radio_link: &RadioLinkLoss) -> String {
    use std::fmt::Write as _;

    let mut line = format!("{} packets", thousands(radio_link.packets));
    for (count, name) in [
        (radio_link.crc_fail, "crc_fail"),
        (radio_link.bad_magic, "bad_magic"),
        (radio_link.seq_gap, "seq_gap"),
        (radio_link.uart_errors, "uart_errors"),
        (radio_link.radio_crc_fail, "radio_crc_fail"),
        (radio_link.radio_bad_magic, "radio_bad_magic"),
        (radio_link.radio_seq_gap, "radio_seq_gap"),
        (radio_link.radio_uart_errors, "radio_uart_errors"),
    ] {
        if count > 0 {
            let _ = write!(line, "  {} {name}", thousands(count));
        }
    }
    line
}

/// The host's figures, each clause only when non-zero but `store dropped`, which is the
/// answer even at 0.
fn host_line(host: &HostLoss) -> String {
    let nonzero = |n: u64, what: &str| (n > 0).then(|| format!("{} {what}", thousands(n)));
    let mut clauses = Vec::new();
    let not_stored = host.duplicates + host.undecodable + host.incompatible;
    if not_stored > 0 {
        let why: Vec<String> = [
            nonzero(host.duplicates, "retries"),
            nonzero(host.undecodable, "undecodable"),
            nonzero(host.incompatible, "incompatible"),
        ]
        .into_iter()
        .flatten()
        .collect();
        clauses.push(format!("{} not stored: {}", thousands(not_stored), why.join("  ")));
    }
    clauses.push(format!("{} store dropped", thousands(host.store_dropped)));
    clauses.extend(nonzero(host.garbled, "garbled"));
    if host.lag_over_100ms > 0 {
        let plural = if host.lag_over_100ms == 1 { "" } else { "s" };
        clauses.push(format!(
            "behind ≥100 ms {} time{plural} (worst {})",
            thousands(host.lag_over_100ms),
            millis(host.lag_peak_us)
        ));
    }
    if host.commit_peak_us > 0 {
        clauses.push(format!("slowest commit {}", millis(host.commit_peak_us)));
    }
    if host.queue_peak > 0 {
        clauses.push(format!("queue peak {}", thousands(host.queue_peak)));
    }
    clauses.join("  ")
}

/// The host's health, or `None` when no row read any. Once the throttle word was read, its
/// counts print even at 0, since a Pi that held up is the answer.
fn health_line(host: &HostLoss) -> Option<String> {
    let mut clauses = Vec::new();
    if let Some(n) = host.under_voltage {
        let plural = if n == 1 { "" } else { "s" };
        clauses.push(format!("under-voltage in {} sample{plural}", thousands(n)));
    }
    if let Some(n) = host.throttled {
        clauses.push(format!("throttled in {}", thousands(n)));
    }
    if let Some(bits) = host.since_boot.filter(|bits| *bits != 0) {
        let events: Vec<&str> =
            ["under-voltage", "frequency capped", "throttled", "soft temperature limit"]
                .into_iter()
                .enumerate()
                .filter(|(bit, _)| bits & (1 << bit) != 0)
                .map(|(_, name)| name)
                .collect();
        clauses.push(format!("since boot: {}", events.join(", ")));
    }
    if let Some(mc) = host.temp_max_mc {
        clauses.push(format!("temp max {:.1} °C", f64::from(mc) / 1000.0));
    }
    if let Some(mv) = host.battery_min_mv {
        clauses.push(format!("battery min {:.2} V", f64::from(mv) / 1000.0));
    }
    (!clauses.is_empty()).then(|| clauses.join("  "))
}

/// Microseconds as milliseconds: one decimal place under 10 ms, whole ones above.
fn millis(us: u64) -> String {
    if us < 10_000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{} ms", thousands((us + 500) / 1000))
    }
}

/// `part` as a share of a non-zero `whole`, to one decimal place.
fn percent(part: u64, whole: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

#[cfg(test)]
mod tests {
    use wartui_core::analyze::{
        BridgeLoss, HostLoss, LossSummary, NodeLoss, NodePeak, Peak, PeakSummary, QueueLoss,
        RadioLinkLoss,
    };
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::link::QueueId;
    use wartui_proto::mac::Mac;

    use super::{NodeNames, losses_text, peaks_text};

    const NODE_1C: Mac = [0x02, 0, 0x5E, 0x10, 0x1C, 0x5A];
    const NODE_57: Mac = [0x02, 0, 0x5E, 0x10, 0x57, 0x84];
    const NODE_90: Mac = [0x02, 0, 0x5E, 0x10, 0x90, 0x01];

    /// Bus A's first three sockets, which are `N1` to `N3` on any carrier.
    const fn a(index: u8) -> Option<Slot> {
        Slot::new(0, index)
    }

    /// `B7`, which is `N14` on the fourteen-node carrier.
    const B7: Option<Slot> = Slot::new(1, 6);

    /// Names for a capture with no node on a slot, so every node goes by its tail.
    fn tails() -> NodeNames {
        NodeNames::new(NodeBoard::Carrier14, &LossSummary::default())
    }

    /// `losses_text` with the names of a capture with no node on a slot.
    fn loss_text(loss: &LossSummary) -> String {
        losses_text(loss, &tails())
    }

    /// `peaks_text` with the names of a capture with no node on a slot.
    fn peak_text(peak: &PeakSummary) -> String {
        peaks_text(peak, &tails())
    }

    /// A loss row with one batch lost.
    fn lost(mac: Mac, slot: Option<Slot>) -> NodeLoss {
        NodeLoss { mac, slot, batches_lost: 1, ..Default::default() }
    }

    /// A summary of `nodes` alone.
    fn node_losses(nodes: Vec<NodeLoss>) -> LossSummary {
        LossSummary { nodes, ..Default::default() }
    }

    /// The rows under `heading`.
    fn rows<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
        text.lines().skip_while(|line| *line != heading).skip(1).collect()
    }

    /// Two nodes over an evening, behind a bridge that restarted once.
    fn evening() -> LossSummary {
        LossSummary {
            bridge: Some(BridgeLoss {
                received: 1_203_551,
                dropped: 1_300,
                reboots: 1,
                host_read: 0,
                usb_lost: 0,
                oversize: 0,
                queues: vec![],
                radio_link: RadioLinkLoss::default(),
                host_uart_errors: 0,
            }),
            nodes: vec![
                NodeLoss {
                    mac: NODE_1C,
                    slot: None,
                    batches_lost: 12,
                    heartbeats: 699,
                    wifi_refused: 0,
                    ble_refused: 1_203,
                    supply_drop: false,
                },
                NodeLoss {
                    mac: NODE_57,
                    slot: None,
                    batches_lost: 200,
                    heartbeats: 2_757,
                    wifi_refused: 18_220,
                    ble_refused: 0,
                    supply_drop: false,
                },
            ],
            host: None,
            ..Default::default()
        }
    }

    /// A drive on a Pi that sagged a few times, with the host falling behind now and then.
    fn drive() -> LossSummary {
        LossSummary {
            bridge: Some(BridgeLoss {
                received: 31_821,
                dropped: 0,
                reboots: 0,
                host_read: 31_821,
                usb_lost: 0,
                oversize: 0,
                queues: vec![],
                radio_link: RadioLinkLoss::default(),
                host_uart_errors: 0,
            }),
            nodes: Vec::new(),
            host: Some(HostLoss {
                duplicates: 140,
                incompatible: 7,
                lag_over_100ms: 31,
                lag_peak_us: 412_300,
                commit_peak_us: 58_000,
                queue_peak: 1_204,
                under_voltage: Some(3),
                throttled: Some(0),
                since_boot: Some(0b0001),
                temp_max_mc: Some(71_234),
                battery_min_mv: Some(3_618),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The same evening's arrival rates.
    fn rates() -> PeakSummary {
        let peak = |per_second, per_minute| Peak { per_second, per_minute };
        PeakSummary {
            sightings: 1_203_551,
            total: peak(1_842, 41_210),
            wifi: peak(1_790, 39_880),
            ble: peak(120, 2_310),
            nodes: vec![
                NodePeak { mac: NODE_1C, slot: None, peak: peak(120, 2_310) },
                NodePeak { mac: NODE_57, slot: None, peak: peak(1_790, 39_880) },
            ],
            clock_reversals: 0,
        }
    }

    #[test]
    fn analyze_report_prints_peak_lines_when_sightings_stored() {
        assert_eq!(
            peak_text(&rates()),
            "  peak       1,842 sightings/s  41,210/min\n\
             \x20 peak wifi  1,790/s  39,880/min\n\
             \x20 peak ble   120/s  2,310/min\n\
             \x20 peak by node\n\
             \x20   1C:5A  120/s  2,310/min\n\
             \x20   57:84  1,790/s  39,880/min\n"
        );
    }

    #[test]
    fn analyze_report_omits_peak_lines_when_no_sightings_stored() {
        assert_eq!(peak_text(&PeakSummary::default()), "");
    }

    #[test]
    fn analyze_report_omits_peak_ble_when_no_ble_sightings() {
        let text = peak_text(&PeakSummary { ble: Peak::default(), ..rates() });
        assert!(!text.contains("peak ble"), "{text}");
        assert!(text.contains("  peak wifi  "), "{text}");
    }

    #[test]
    fn analyze_report_names_clock_reversals_when_wall_clock_steps_back() {
        let text = peak_text(&PeakSummary { clock_reversals: 1, ..rates() });
        assert!(
            text.starts_with(
                "  peak       1,842 sightings/s  41,210/min  wall clock reversed 1 time\n"
            ),
            "{text}"
        );
        let text = peak_text(&PeakSummary { clock_reversals: 2, ..rates() });
        assert!(text.contains("41,210/min  wall clock reversed 2 times\n"), "{text}");
    }

    #[test]
    fn analyze_report_lists_node_lines_when_nodes_present() {
        let text = loss_text(&evening());
        assert!(
            text.contains("\n    1C:5A  batches 12  heartbeats 699  buffer full ble 1,203\n"),
            "{text}"
        );
        assert!(
            text.contains("\n    57:84  batches 200  heartbeats 2,757  buffer full wifi 18,220\n"),
            "{text}"
        );
        assert!(text.contains("  batches    212 lost between node and host\n"), "{text}");
        assert!(
            text.starts_with(
                "  bridge     1,203,551 frames received  1,300 dropped (0.1%)  1 reboot\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn analyze_report_omits_bridge_line_when_no_status_rows() {
        let mut loss = evening();
        loss.bridge = None;
        let text = loss_text(&loss);
        assert!(!text.contains("bridge"), "{text}");
        assert!(text.starts_with("  batches "), "{text}");
    }

    #[test]
    fn analyze_report_omits_reboot_clause_when_bridge_never_restarted() {
        let mut loss = evening();
        loss.bridge = Some(BridgeLoss {
            received: 10,
            dropped: 0,
            reboots: 0,
            host_read: 0,
            usb_lost: 0,
            oversize: 0,
            queues: vec![],
            radio_link: RadioLinkLoss::default(),
            host_uart_errors: 0,
        });
        let text = loss_text(&loss);
        assert!(text.starts_with("  bridge     10 frames received  0 dropped (0.0%)\n"), "{text}");
    }

    /// A bridge line for `received`, `dropped` and `host_read`, `usb_lost` taken as
    /// analyze takes it.
    fn bridge_line(received: u64, dropped: u64, host_read: u64) -> String {
        let usb_lost = received.saturating_sub(dropped).saturating_sub(host_read);
        let loss = LossSummary {
            bridge: Some(BridgeLoss {
                received,
                dropped,
                reboots: 0,
                host_read,
                usb_lost,
                oversize: 0,
                queues: vec![],
                radio_link: RadioLinkLoss::default(),
                host_uart_errors: 0,
            }),
            ..Default::default()
        };
        first_line(&loss_text(&loss))
    }

    /// The report's first line, the bridge's, with its newline.
    fn first_line(text: &str) -> String {
        text.split_inclusive('\n').next().unwrap_or_default().to_owned()
    }

    #[test]
    fn analyze_report_prints_oversize_clause_when_bridge_dropped_oversize_frames() {
        let loss = LossSummary {
            bridge: Some(BridgeLoss {
                received: 1_000,
                dropped: 10,
                reboots: 0,
                host_read: 990,
                usb_lost: 0,
                oversize: 42,
                queues: vec![],
                radio_link: RadioLinkLoss::default(),
                host_uart_errors: 0,
            }),
            ..Default::default()
        };
        assert_eq!(
            first_line(&loss_text(&loss)),
            "  bridge     1,000 frames received  10 dropped (1.0%)  host read 990  \
             42 oversize (not a node's)\n"
        );
    }

    #[test]
    fn analyze_report_omits_oversize_clause_when_bridge_dropped_none() {
        let text = bridge_line(1_000, 10, 990);
        assert!(!text.contains("oversize"), "{text}");
    }

    #[test]
    fn analyze_report_prints_usb_clause_when_frames_lost_on_usb() {
        let text = bridge_line(1_000, 10, 985);
        assert_eq!(
            text,
            "  bridge     1,000 frames received  10 dropped (1.0%)  host read 985  5 lost on USB\n"
        );
    }

    #[test]
    fn analyze_report_omits_usb_clause_when_host_read_all_the_bridge_sent() {
        let text = bridge_line(1_000, 10, 990);
        assert_eq!(text, "  bridge     1,000 frames received  10 dropped (1.0%)  host read 990\n");
    }

    #[test]
    fn analyze_report_omits_usb_clause_when_host_read_exceeds_what_the_bridge_sent() {
        // A baseline taken while a backlog drained: the host reads frames the first reply
        // overtook.
        let text = bridge_line(1_000, 10, 1_004);
        assert_eq!(
            text,
            "  bridge     1,000 frames received  10 dropped (1.0%)  host read 1,004\n"
        );
    }

    /// A bridge's line, two of its queues and its radio link.
    fn bridge_with_link(radio_link: RadioLinkLoss) -> LossSummary {
        LossSummary {
            bridge: Some(BridgeLoss {
                received: 1_000,
                dropped: 0,
                reboots: 0,
                host_read: 1_000,
                usb_lost: 0,
                oversize: 0,
                queues: vec![
                    QueueLoss {
                        queue: QueueId::HostBulk,
                        capacity: 262_144,
                        peak: 1_024,
                        dropped: 0,
                    },
                    QueueLoss { queue: QueueId::NodeDown, capacity: 4096, peak: 3102, dropped: 3 },
                ],
                radio_link,
                host_uart_errors: 0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn analyze_report_prints_queue_lines_when_bridge_reports_queues() {
        assert_eq!(
            loss_text(&bridge_with_link(RadioLinkLoss { packets: 9, ..Default::default() })),
            "  bridge     1,000 frames received  0 dropped (0.0%)  host read 1,000\n\
             \x20 queue      host-bulk  peak 1,024/262,144 bytes  0 dropped\n\
             \x20 queue      node-down  peak 3,102/4,096 bytes  3 dropped\n\
             \x20 radio link 9 packets\n\
             \x20 host link  0 host_uart_errors\n"
        );
    }

    #[test]
    fn analyze_report_prints_radio_link_errors_only_when_nonzero() {
        let radio_link = RadioLinkLoss {
            packets: 48_210,
            crc_fail: 2,
            uart_errors: 1,
            radio_seq_gap: 4,
            ..Default::default()
        };
        let text = loss_text(&bridge_with_link(radio_link));
        assert!(
            text.contains(
                "  radio link 48,210 packets  2 crc_fail  1 uart_errors  4 radio_seq_gap\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn analyze_report_prints_radio_link_packets_alone_when_link_is_clean() {
        let text = loss_text(&bridge_with_link(RadioLinkLoss { packets: 9, ..Default::default() }));
        assert!(text.contains("  radio link 9 packets\n"), "{text}");
    }

    #[test]
    fn analyze_report_prints_host_uart_errors_even_at_zero_when_none_occurred() {
        let mut loss = bridge_with_link(RadioLinkLoss { packets: 9, ..Default::default() });
        assert!(loss_text(&loss).ends_with("  host link  0 host_uart_errors\n"));
        loss.bridge.as_mut().unwrap().host_uart_errors = 1_204;
        let text = loss_text(&loss);
        assert!(text.ends_with("  host link  1,204 host_uart_errors\n"), "{text}");
    }

    #[test]
    fn analyze_report_omits_buffer_full_figures_when_zero() {
        let mut loss = evening();
        for node in &mut loss.nodes {
            node.wifi_refused = 0;
            node.ble_refused = 0;
        }
        let text = loss_text(&loss);
        assert!(!text.contains("buffer full"), "{text}");
        assert!(text.contains("\n    1C:5A  batches 12  heartbeats 699\n"), "{text}");

        let text = loss_text(&evening());
        assert!(text.contains("  buffer full wifi 18,220  ble 1,203\n"), "{text}");
    }

    #[test]
    fn analyze_report_prints_host_and_health_lines_when_host_rows_present() {
        let text = loss_text(&drive());
        assert_eq!(
            text,
            "  bridge     31,821 frames received  0 dropped (0.0%)  host read 31,821\n\
             \x20 radio link 0 packets\n\
             \x20 host link  0 host_uart_errors\n\
             \x20 host       147 not stored: 140 retries  7 incompatible  0 store dropped  \
             behind ≥100 ms 31 times (worst 412 ms)  slowest commit 58 ms  queue peak 1,204\n\
             \x20 health     under-voltage in 3 samples  throttled in 0  \
             since boot: under-voltage  temp max 71.2 °C  battery min 3.62 V\n"
        );
    }

    #[test]
    fn analyze_report_omits_battery_clause_when_no_battery_read() {
        let mut loss = drive();
        if let Some(host) = &mut loss.host {
            host.battery_min_mv = None;
        }
        let text = loss_text(&loss);
        assert!(text.contains("temp max 71.2 °C\n"), "{text}");
        assert!(!text.contains("battery"), "{text}");
    }

    #[test]
    fn analyze_report_prints_store_dropped_alone_when_host_figures_are_zero() {
        let mut loss = drive();
        loss.host = Some(HostLoss::default());
        let text = loss_text(&loss);
        assert!(text.contains("\n  host       0 store dropped\n"), "{text}");
        assert!(!text.contains("health "), "no health column was read: {text}");
    }

    #[test]
    fn analyze_report_prints_temperature_alone_when_host_is_not_a_pi() {
        let mut loss = drive();
        loss.host = Some(HostLoss { temp_max_mc: Some(48_000), ..Default::default() });
        let text = loss_text(&loss);
        assert!(text.ends_with("\n  health     temp max 48.0 °C\n"), "{text}");
    }

    #[test]
    fn analyze_report_names_every_event_when_since_boot_bits_set() {
        let mut loss = drive();
        if let Some(host) = &mut loss.host {
            host.since_boot = Some(0b1110);
        }
        let text = loss_text(&loss);
        assert!(
            text.contains("  since boot: frequency capped, throttled, soft temperature limit  "),
            "{text}"
        );
    }

    #[test]
    fn analyze_report_omits_host_lines_when_no_host_rows() {
        let text = loss_text(&evening());
        assert!(!text.contains("  host       "), "{text}");
        assert!(!text.contains("health "), "{text}");
    }

    #[test]
    fn analyze_report_names_nodes_by_label_when_on_a_carrier() {
        let loss = node_losses(vec![lost(NODE_57, a(0)), lost(NODE_1C, a(1))]);
        let names = NodeNames::new(NodeBoard::Carrier14, &loss);
        assert_eq!(
            rows(&losses_text(&loss, &names), "  loss by node"),
            ["    N1  batches 1", "    N2  batches 1"]
        );
    }

    #[test]
    fn analyze_report_names_node_by_tail_when_it_has_no_slot() {
        let loss = node_losses(vec![lost(NODE_57, a(0)), lost(NODE_1C, None)]);
        let names = NodeNames::new(NodeBoard::Carrier14, &loss);
        assert_eq!(
            rows(&losses_text(&loss, &names), "  loss by node"),
            ["    N1     batches 1", "    1C:5A  batches 1"]
        );
    }

    #[test]
    fn analyze_report_marks_only_the_dropped_node_when_a_supply_drop_is_recorded() {
        let dropped = NodeLoss { supply_drop: true, ..lost(NODE_1C, a(1)) };
        let loss = node_losses(vec![lost(NODE_57, a(0)), dropped]);
        let names = NodeNames::new(NodeBoard::Carrier14, &loss);
        let text = losses_text(&loss, &names);
        assert_eq!(
            rows(&text, "  loss by node"),
            ["    N1  batches 1", "    N2  batches 1  supply drop"]
        );
        assert!(!text.contains(" V"), "no voltage figure: {text}");
    }

    #[test]
    fn analyze_report_keeps_columns_aligned_when_label_widths_differ() {
        let loss = node_losses(vec![lost(NODE_57, a(0)), lost(NODE_1C, B7)]);
        let names = NodeNames::new(NodeBoard::Carrier14, &loss);
        assert_eq!(
            rows(&losses_text(&loss, &names), "  loss by node"),
            ["    N1   batches 1", "    N14  batches 1"]
        );
    }

    #[test]
    fn analyze_report_adds_tail_only_to_rows_sharing_a_slot_when_another_node_took_it() {
        let loss = node_losses(vec![lost(NODE_1C, a(2)), lost(NODE_57, a(2)), lost(NODE_90, B7)]);
        let names = NodeNames::new(NodeBoard::Carrier14, &loss);
        assert_eq!(
            rows(&losses_text(&loss, &names), "  loss by node"),
            ["    N3 1C:5A  batches 1", "    N3 57:84  batches 1", "    N14       batches 1"]
        );
    }

    #[test]
    fn analyze_report_adds_tail_in_peak_rows_when_slot_is_shared_with_a_node_without_sightings() {
        // The other node has no peak row, and the two sections should still name this one alike.
        let loss = node_losses(vec![lost(NODE_1C, a(2)), lost(NODE_57, a(2))]);
        let names = NodeNames::new(NodeBoard::Carrier14, &loss);
        let peak = PeakSummary {
            sightings: 1,
            nodes: vec![NodePeak {
                mac: NODE_57,
                slot: a(2),
                peak: Peak { per_second: 1, per_minute: 1 },
            }],
            ..Default::default()
        };
        assert_eq!(
            rows(&peaks_text(&peak, &names), "  peak by node"),
            ["    N3 57:84  1/s  1/min"]
        );
    }
}
