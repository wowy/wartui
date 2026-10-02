//! `wartui analyze` — what a capture holds, and what it lost on the way in.
//!
//! The first half is what `export` reports, from the same fold run into nothing, so the
//! counts match what an export of the same selection would write. The second half is
//! `wartui_core::analyze`, whose `//!` says how each figure is counted. Standard output,
//! because nothing else is written there.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use wartui_core::analyze::{LossSummary, NodeLoss, losses};
use wartui_core::export::wigle_csv;
use wartui_core::store::open_readonly;

use crate::export::{Selection, details, note_unpositioned, short_mac, thousands};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,
}

pub fn run(args: Args) -> Result<()> {
    let db = args.selection.capture("analyzing")?;
    let conn = open_readonly(&db).with_context(|| format!("opening {}", db.display()))?;
    let filter = args.selection.filter();
    let summary = wigle_csv(&conn, filter, &mut std::io::sink(), env!("CARGO_PKG_VERSION"))?;
    let loss = losses(&conn).context("reading what the capture lost")?;
    print!(
        "{} rows to export\n{}{}",
        thousands(summary.rows),
        details(&summary),
        losses_text(&loss)
    );
    note_unpositioned(&summary);
    Ok(())
}

/// The loss lines, in the layout of [`details`]. Every figure is an exact count.
fn losses_text(loss: &LossSummary) -> String {
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
        let _ = writeln!(text, "{line}");
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
    let (beats, missed) = (total(|n| n.heartbeats), total(|n| n.heartbeats_missed));
    if beats > 0 {
        let expected = beats + missed;
        let _ = writeln!(
            text,
            "  {:<11}{} missed of {} expected ({})",
            "heartbeats",
            thousands(missed),
            thousands(expected),
            percent(missed, expected)
        );
    }
    let refused = ring(total(|n| n.wifi_refused), total(|n| n.ble_refused), " refused");
    if !refused.is_empty() {
        let _ = writeln!(text, "  {:<11}{}", "ring", refused.join("  "));
    }

    let _ = writeln!(text, "  loss by node");
    for node in &loss.nodes {
        let mut line =
            format!("    {}  batches {}", short_mac(&node.mac), thousands(node.batches_lost));
        if node.heartbeats > 0 {
            let _ = write!(
                line,
                "  heartbeats {}/{}",
                thousands(node.heartbeats_missed),
                thousands(node.heartbeats + node.heartbeats_missed)
            );
        }
        let refused = ring(node.wifi_refused, node.ble_refused, "");
        if !refused.is_empty() {
            let _ = write!(line, "  ring {}", refused.join("  "));
        }
        let _ = writeln!(text, "{line}");
    }
    text
}

/// The non-zero ring refusal figures, each followed by `suffix`.
fn ring(wifi: u64, ble: u64, suffix: &str) -> Vec<String> {
    [("wifi", wifi), ("ble", ble)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(kind, n)| format!("{kind} {}{suffix}", thousands(n)))
        .collect()
}

/// `part` as a share of a non-zero `whole`, to one decimal place.
fn percent(part: u64, whole: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

#[cfg(test)]
mod tests {
    use wartui_core::analyze::{BridgeLoss, LossSummary, NodeLoss};

    use super::losses_text;

    /// Two nodes over an evening, behind a bridge that restarted once.
    fn evening() -> LossSummary {
        LossSummary {
            bridge: Some(BridgeLoss { received: 1_203_551, dropped: 1_300, reboots: 1 }),
            nodes: vec![
                NodeLoss {
                    mac: vec![0x02, 0, 0x5E, 0x10, 0x1C, 0x5A],
                    batches_lost: 12,
                    heartbeats: 699,
                    heartbeats_missed: 3,
                    wifi_refused: 0,
                    ble_refused: 1_203,
                },
                NodeLoss {
                    mac: vec![0x02, 0, 0x5E, 0x10, 0x57, 0x84],
                    batches_lost: 200,
                    heartbeats: 2_757,
                    heartbeats_missed: 38,
                    wifi_refused: 18_220,
                    ble_refused: 0,
                },
            ],
        }
    }

    #[test]
    fn analyze_report_lists_node_lines_when_nodes_present() {
        let text = losses_text(&evening());
        assert!(
            text.contains("\n    1C:5A  batches 12  heartbeats 3/702  ring ble 1,203\n"),
            "{text}"
        );
        assert!(
            text.contains("\n    57:84  batches 200  heartbeats 38/2,795  ring wifi 18,220\n"),
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
    fn analyze_report_prints_heartbeats_as_plain_counts_when_any_reported() {
        let text = losses_text(&evening());
        assert!(text.contains("  heartbeats 41 missed of 3,497 expected (1.2%)\n"), "{text}");
        assert!(!text.contains('~'), "{text}");
    }

    #[test]
    fn analyze_report_omits_bridge_line_when_no_status_rows() {
        let mut loss = evening();
        loss.bridge = None;
        let text = losses_text(&loss);
        assert!(!text.contains("bridge"), "{text}");
        assert!(text.starts_with("  batches "), "{text}");
    }

    #[test]
    fn analyze_report_omits_reboot_clause_when_bridge_never_restarted() {
        let mut loss = evening();
        loss.bridge = Some(BridgeLoss { received: 10, dropped: 0, reboots: 0 });
        let text = losses_text(&loss);
        assert!(text.starts_with("  bridge     10 frames received  0 dropped (0.0%)\n"), "{text}");
    }

    #[test]
    fn analyze_report_omits_ring_figures_when_zero() {
        let mut loss = evening();
        for node in &mut loss.nodes {
            node.wifi_refused = 0;
            node.ble_refused = 0;
        }
        let text = losses_text(&loss);
        assert!(!text.contains("ring"), "{text}");
        assert!(text.contains("\n    1C:5A  batches 12  heartbeats 3/702\n"), "{text}");

        let text = losses_text(&evening());
        assert!(text.contains("  ring       wifi 18,220 refused  ble 1,203 refused\n"), "{text}");
    }
}
