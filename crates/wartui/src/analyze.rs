//! `wartui analyze` — what a capture holds, and what it lost on the way in.
//!
//! The first half is what `export` reports, from the same fold run into nothing, so the
//! counts match what an export of the same selection would write. The second half is
//! `wartui_core::analyze`, whose `//!` says how each figure is counted. Standard output,
//! because nothing else is written there.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Args as ClapArgs;
use wartui_core::analyze::{HostLoss, LossSummary, NodeLoss, losses};
use wartui_core::export::wigle_csv;
use wartui_core::store::open_readonly;
use wartui_proto::mac;

use crate::export::{
    Selection, details, note_quality, note_unknown_kind, note_unpositioned, thousands,
};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,
    /// Show UTC arrival brackets for missing heartbeats, including uncertain restart intervals.
    #[arg(long)]
    heartbeat_windows: bool,
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
    if args.heartbeat_windows {
        print!("{}", heartbeat_windows_text(&loss));
    }
    note_unpositioned(&summary);
    note_unknown_kind(&summary);
    note_quality(&summary);
    Ok(())
}

/// The loss lines, in the layout of [`details`]. Heartbeats follow the core's modulo assumptions;
/// `lost on USB` is approximate to the bridge's queued frames.
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
        if bridge.host_read > 0 {
            let _ = write!(line, "  host read {}", thousands(bridge.host_read));
        }
        if bridge.usb_lost > 0 {
            let _ = write!(line, "  {} lost on USB", thousands(bridge.usb_lost));
        }
        let _ = writeln!(text, "{line}");
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
    let (beats, missed) = (total(|n| n.heartbeats), total(|n| n.heartbeats_missed));
    if beats > 0 {
        let expected = beats + missed;
        let _ = write!(
            text,
            "  {:<11}{} missed of {} expected ({})",
            "heartbeats",
            thousands(missed),
            thousands(expected),
            percent(missed, expected)
        );
        let unsent = total(|n| n.heartbeats_unsent);
        if unsent > 0 {
            let _ = write!(text, "  {} unsent", thousands(unsent));
        }
        text.push('\n');
    }
    // `buffer full` fills the label column, so it carries its own space.
    if let Some(figures) = buffer_full(total(|n| n.wifi_refused), total(|n| n.ble_refused)) {
        let _ = writeln!(text, "  {figures}");
    }

    let _ = writeln!(text, "  loss by node");
    for node in &loss.nodes {
        let mut line =
            format!("    {}  batches {}", mac::short(&node.mac), thousands(node.batches_lost));
        if node.heartbeats > 0 {
            let _ = write!(
                line,
                "  heartbeats {}/{} ({})",
                thousands(node.heartbeats_missed),
                thousands(node.heartbeats + node.heartbeats_missed),
                percent(node.heartbeats_missed, node.heartbeats + node.heartbeats_missed)
            );
            if node.heartbeats_unsent > 0 {
                let _ = write!(line, "  {} unsent", thousands(node.heartbeats_unsent));
            }
        }
        if let Some(figures) = buffer_full(node.wifi_refused, node.ble_refused) {
            let _ = write!(line, "  {figures}");
        }
        let _ = writeln!(text, "{line}");
    }
    text
}

/// Arrival brackets keep row order; wall-clock reversals must not look like elapsed time.
fn heartbeat_windows_text(loss: &LossSummary) -> String {
    use std::fmt::Write as _;

    let mut text =
        String::from("  heartbeat windows (UTC arrival brackets, not transmission times)\n");
    let mut any = false;
    for node in &loss.nodes {
        for window in &node.heartbeat_windows {
            any = true;
            let _ = write!(
                text,
                "    {}  {} -> {}  rows {} -> {}  {} missed",
                mac::short(&node.mac),
                utc_arrival(window.start_rx_at_ms),
                utc_arrival(window.end_rx_at_ms),
                window.start_id,
                window.end_id,
                thousands(window.missed)
            );
            if window.restarted {
                text.push_str(" since boot; restart-associated, uncertain interval");
            }
            if window.unsent > 0 {
                let _ = write!(text, " ({} unsent)", thousands(window.unsent));
            }
            if window.end_rx_at_ms < window.start_rx_at_ms {
                text.push_str("; wall clock reversed");
            }
            text.push('\n');
        }
    }
    if !any {
        text.push_str("    no counted gaps\n");
    }
    text
}

fn utc_arrival(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || format!("{ms} unix ms (outside UTC date range)"),
        |at| at.to_rfc3339_opts(SecondsFormat::Millis, true),
    )
}

/// The host's figures, each clause only when non-zero but `store dropped`, which is the
/// answer even at 0.
fn host_line(host: &HostLoss) -> String {
    let nonzero = |n: u64, what: &str| (n > 0).then(|| format!("{} {what}", thousands(n)));
    let mut clauses = Vec::new();
    let not_stored = host.duplicates + host.undecodable + host.foreign;
    if not_stored > 0 {
        let why: Vec<String> = [
            nonzero(host.duplicates, "retries"),
            nonzero(host.undecodable, "undecodable"),
            nonzero(host.foreign, "foreign"),
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
    clauses.extend(nonzero(host.admin_windows_missed, "admin windows missed"));
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

/// `buffer full` and the non-zero counts of sightings a full pending buffer refused, or
/// `None` when both are zero. The TUI footer prints the same text.
pub(crate) fn buffer_full(wifi: u64, ble: u64) -> Option<String> {
    let figures: Vec<String> = [("wifi", wifi), ("ble", ble)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(kind, n)| format!("{kind} {}", thousands(n)))
        .collect();
    (!figures.is_empty()).then(|| format!("buffer full {}", figures.join("  ")))
}

/// `part` as a share of a non-zero `whole`, to one decimal place.
fn percent(part: u64, whole: u64) -> String {
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use wartui_core::analyze::{BridgeLoss, HeartbeatWindow, HostLoss, LossSummary, NodeLoss};

    use super::{heartbeat_windows_text, losses_text, utc_arrival};

    /// Two nodes over an evening, behind a bridge that restarted once.
    fn evening() -> LossSummary {
        LossSummary {
            bridge: Some(BridgeLoss {
                received: 1_203_551,
                dropped: 1_300,
                reboots: 1,
                host_read: 0,
                usb_lost: 0,
            }),
            nodes: vec![
                NodeLoss {
                    mac: [0x02, 0, 0x5E, 0x10, 0x1C, 0x5A],
                    batches_lost: 12,
                    heartbeats: 699,
                    heartbeats_missed: 3,
                    heartbeats_unsent: 0,
                    wifi_refused: 0,
                    ble_refused: 1_203,
                    heartbeat_windows: Vec::new(),
                },
                NodeLoss {
                    mac: [0x02, 0, 0x5E, 0x10, 0x57, 0x84],
                    batches_lost: 200,
                    heartbeats: 2_757,
                    heartbeats_missed: 38,
                    heartbeats_unsent: 0,
                    wifi_refused: 18_220,
                    ble_refused: 0,
                    heartbeat_windows: Vec::new(),
                },
            ],
            host: None,
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
            }),
            nodes: Vec::new(),
            host: Some(HostLoss {
                duplicates: 140,
                foreign: 7,
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
        }
    }

    #[test]
    fn analyze_report_lists_node_lines_when_nodes_present() {
        let text = losses_text(&evening());
        assert!(
            text.contains(
                "\n    1C:5A  batches 12  heartbeats 3/702 (0.4%)  buffer full ble 1,203\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "\n    57:84  batches 200  heartbeats 38/2,795 (1.4%)  buffer full wifi 18,220\n"
            ),
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
    fn analyze_report_prints_node_loss_rates_when_nodes_have_heartbeats() {
        let text = losses_text(&evening());
        assert!(text.contains("heartbeats 3/702 (0.4%)"), "{text}");
        assert!(text.contains("heartbeats 38/2,795 (1.4%)"), "{text}");
    }

    #[test]
    fn analyze_report_prints_unsent_heartbeats_when_node_refused_broadcasts() {
        let mut loss = evening();
        loss.nodes[1].heartbeats_unsent = 12;
        loss.nodes[1].heartbeat_windows = vec![HeartbeatWindow {
            start_id: 3,
            end_id: 7,
            start_rx_at_ms: 1_000,
            end_rx_at_ms: 16_123,
            missed: 3,
            unsent: 1,
            restarted: false,
        }];
        let text = losses_text(&loss);
        assert!(
            text.contains("  heartbeats 41 missed of 3,497 expected (1.2%)  12 unsent\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "\n    57:84  batches 200  heartbeats 38/2,795 (1.4%)  12 unsent  \
                 buffer full wifi 18,220\n"
            ),
            "{text}"
        );
        // A node whose radio refused nothing says nothing about it.
        assert!(
            text.contains("\n    1C:5A  batches 12  heartbeats 3/702 (0.4%)  buffer full ble"),
            "{text}"
        );
        let text = heartbeat_windows_text(&loss);
        assert!(text.contains("rows 3 -> 7  3 missed (1 unsent)\n"), "{text}");
    }

    #[test]
    fn analyze_enables_windows_when_flag_is_present() {
        for (flags, expected) in [
            (vec!["wartui", "analyze", "--db", "synthetic.db"], false),
            (vec!["wartui", "analyze", "--db", "synthetic.db", "--heartbeat-windows"], true),
        ] {
            let cli = crate::Cli::try_parse_from(flags).unwrap();
            let Some(crate::Command::Analyze(args)) = cli.command else { panic!("analyze") };
            assert_eq!(args.heartbeat_windows, expected);
        }
    }

    #[test]
    fn analyze_report_brackets_gaps_when_windows_requested() {
        let mut loss = evening();
        loss.nodes[0].heartbeat_windows = vec![HeartbeatWindow {
            start_id: 3,
            end_id: 7,
            start_rx_at_ms: 1_000,
            end_rx_at_ms: 16_123,
            missed: 3,
            unsent: 0,
            restarted: false,
        }];
        assert!(!losses_text(&loss).contains("1970-"));
        assert_eq!(
            heartbeat_windows_text(&loss),
            "  heartbeat windows (UTC arrival brackets, not transmission times)\n\
             \x20   1C:5A  1970-01-01T00:00:01.000Z -> 1970-01-01T00:00:16.123Z  rows 3 -> 7  3 missed\n"
        );
    }

    #[test]
    fn analyze_report_marks_uncertainty_when_window_crosses_restart_and_clock_reversal() {
        let mut loss = evening();
        loss.nodes[1].heartbeat_windows = vec![HeartbeatWindow {
            start_id: 10,
            end_id: 11,
            start_rx_at_ms: 1_000,
            end_rx_at_ms: -1_000,
            missed: 11,
            unsent: 3,
            restarted: true,
        }];
        let text = heartbeat_windows_text(&loss);
        assert!(text.contains("1970-01-01T00:00:01.000Z -> 1969-12-31T23:59:59.000Z"), "{text}");
        // "since boot" qualifies the missed count, so the unsent clause follows the restart text.
        assert!(text.contains("rows 10 -> 11  11 missed since boot; restart-associated, uncertain interval (3 unsent); wall clock reversed\n"), "{text}");
    }

    #[test]
    fn analyze_report_prints_no_gaps_when_windows_empty() {
        let text = heartbeat_windows_text(&evening());
        assert!(text.ends_with("    no counted gaps\n"), "{text}");
        assert_eq!(heartbeat_windows_text(&LossSummary::default()), text);
    }

    #[test]
    fn analyze_report_preserves_timestamp_when_utc_date_is_out_of_range() {
        assert_eq!(utc_arrival(i64::MAX), format!("{} unix ms (outside UTC date range)", i64::MAX));
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
        loss.bridge =
            Some(BridgeLoss { received: 10, dropped: 0, reboots: 0, host_read: 0, usb_lost: 0 });
        let text = losses_text(&loss);
        assert!(text.starts_with("  bridge     10 frames received  0 dropped (0.0%)\n"), "{text}");
    }

    /// A bridge line for `received`, `dropped` and `host_read`, `usb_lost` taken as
    /// analyze takes it.
    fn bridge_line(received: u64, dropped: u64, host_read: u64) -> String {
        let usb_lost = received.saturating_sub(dropped).saturating_sub(host_read);
        let loss = LossSummary {
            bridge: Some(BridgeLoss { received, dropped, reboots: 0, host_read, usb_lost }),
            ..Default::default()
        };
        losses_text(&loss)
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

    #[test]
    fn analyze_report_omits_buffer_full_figures_when_zero() {
        let mut loss = evening();
        for node in &mut loss.nodes {
            node.wifi_refused = 0;
            node.ble_refused = 0;
        }
        let text = losses_text(&loss);
        assert!(!text.contains("buffer full"), "{text}");
        assert!(text.contains("\n    1C:5A  batches 12  heartbeats 3/702 (0.4%)\n"), "{text}");

        let text = losses_text(&evening());
        assert!(text.contains("  buffer full wifi 18,220  ble 1,203\n"), "{text}");
    }

    #[test]
    fn analyze_report_prints_host_and_health_lines_when_host_rows_present() {
        let text = losses_text(&drive());
        assert_eq!(
            text,
            "  bridge     31,821 frames received  0 dropped (0.0%)  host read 31,821\n\
             \x20 host       147 not stored: 140 retries  7 foreign  0 store dropped  \
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
        let text = losses_text(&loss);
        assert!(text.contains("temp max 71.2 °C\n"), "{text}");
        assert!(!text.contains("battery"), "{text}");
    }

    #[test]
    fn analyze_report_prints_store_dropped_alone_when_host_figures_are_zero() {
        let mut loss = drive();
        loss.host = Some(HostLoss::default());
        let text = losses_text(&loss);
        assert!(text.contains("\n  host       0 store dropped\n"), "{text}");
        assert!(!text.contains("health "), "no health column was read: {text}");
    }

    #[test]
    fn analyze_report_prints_temperature_alone_when_host_is_not_a_pi() {
        let mut loss = drive();
        loss.host = Some(HostLoss { temp_max_mc: Some(48_000), ..Default::default() });
        let text = losses_text(&loss);
        assert!(text.ends_with("\n  health     temp max 48.0 °C\n"), "{text}");
    }

    #[test]
    fn analyze_report_names_every_event_when_since_boot_bits_set() {
        let mut loss = drive();
        if let Some(host) = &mut loss.host {
            host.since_boot = Some(0b1110);
        }
        let text = losses_text(&loss);
        assert!(
            text.contains("  since boot: frequency capped, throttled, soft temperature limit  "),
            "{text}"
        );
    }

    #[test]
    fn analyze_report_omits_host_lines_when_no_host_rows() {
        let text = losses_text(&evening());
        assert!(!text.contains("host "), "{text}");
        assert!(!text.contains("health "), "{text}");
    }
}
