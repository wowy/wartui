//! The bridge's panel, composed here.
//!
//! A bridge with a screen is sent finished lines and blits them. It never composes
//! one, never parses an air frame to find out what to say, and owns nothing about the
//! panel but the three colours a [`Severity`] maps to. That is the same division that
//! keeps the planner on the host: everything likely to be wrong lives where `cargo
//! test` can reach it and a fix costs a `cargo run`, and what is left on the board is
//! small enough to be finished.
//!
//! So this module is a pure function of one [`Snapshot`] and the geometry the bridge
//! announced. No clock — a snapshot already carries the only times that matter, and
//! the liveness question a line would otherwise ask was answered by the engine and
//! carried in [`crate::engine::NodeView`]. No I/O, and nothing here decides *when* a
//! panel is pushed; [`crate::runtime`] owns that.
//!
//! Five lines of the eight a panel has, which leaves room to add one without
//! rearranging anything. Each is coloured by its own state, so the panel reads at a
//! glance without being read.
//!
//! # The GPS line is green only on a live, current fix
//!
//! Stricter than the view's reading of the same facts, and deliberately so. [`crate::gps`]
//! argues that a receiver nobody attached is not a *fault*, and it is not — but this
//! line is grading the capture rather than the subsystem, and a capture being written
//! against a constant position is the thing an operator most needs to notice from
//! across a car. So [`PositionSource::Gps`] and nothing else is green.
//!
//! What is left splits on whether a receiver is contributing at all. One that is
//! attached and trying — connecting, scanning, searching, or holding a fix the chain
//! has stopped believing — is working but not ideal, and yellow. Nothing attached, a
//! port that will not read, and a pinned `--lat`/`--lon` are red however deliberate
//! any of them was, because none of the three is going to produce another fix.
//!
//! # The RSSI line is link health, not signal strength
//!
//! [`crate::engine::NodeState::link_rssi`] is how strongly *the bridge* heard a node,
//! not how strongly a node heard an access point. It is the number that says whether
//! an assignment will land, which is why it is worth one of five lines.

use wartui_proto::link::{Panel, PanelLine, PanelLines, Severity, ShortStr};

use crate::engine::Snapshot;
use crate::gps::GpsStatus;
use crate::position::PositionSource;

/// Mean link RSSI, in dBm, below which the fleet is worth looking at.
///
/// Not a stock link budget. Every radio in the fleet transmits at 2 dBm
/// ([`wartui_proto::plan::TX_POWER_QUARTER_DBM`]) and ESP-NOW goes out at 802.11g
/// 24 Mbps, whose specified receiver sensitivity is about −74 dBm — so that is the
/// floor, and a threshold set at it would warn at the moment nodes started missing
/// their admin windows rather than before. These two sit above it on purpose, while
/// there is still something an operator can do: close a window, move the dongle off
/// the floor, walk a node back.
pub const RSSI_WEAK_AVG: i8 = -65;

/// The reading below which a figure stops being worth printing.
///
/// Well under both thresholds, because this is not a judgement about the link —
/// [`RSSI_WEAK_MIN`] already makes that one — but about the number. See [`dbm`].
const RSSI_FLOOR: i32 = -100;

/// Weakest link RSSI, in dBm, below which one node is worth looking at.
///
/// The more negative of the pair, because one node at the edge of a fleet that is
/// otherwise fine is exactly the case an average cannot show. See [`RSSI_WEAK_AVG`]
/// for where both numbers come from.
pub const RSSI_WEAK_MIN: i8 = -70;

/// Lay out a panel for one snapshot.
///
/// The lines are truncated to `panel.cols` and capped at `panel.rows`, so a narrower
/// or shorter screen loses the end of a line rather than overflowing it. The caller
/// sends the result verbatim.
#[must_use]
pub fn render(snapshot: &Snapshot, panel: Panel) -> PanelLines {
    let composed = [
        gps_line(snapshot),
        nodes_line(snapshot),
        aps_line(snapshot),
        ble_line(snapshot),
        rssi_line(snapshot),
    ];

    let mut lines = PanelLines::new();
    for (level, text) in composed.into_iter().take(panel.rows as usize) {
        // The vector's own capacity is `PANEL_ROWS`, so a bridge claiming more rows
        // than the wire can carry gets what fits rather than a truncated frame.
        if lines.push(PanelLine { level, text: fit(&text, panel.cols) }).is_err() {
            break;
        }
    }
    lines
}

/// One line's worth of text, cut to the panel's width.
///
/// By characters rather than bytes: everything composed here is ASCII today, and a
/// slice at a byte that is not a boundary would be a panic rather than a short line
/// the first time that stopped being true.
fn fit(text: &str, cols: u8) -> ShortStr {
    let mut out = ShortStr::new();
    for c in text.chars().take(cols as usize) {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

/// Where the position is coming from, and whether anything is still trying.
fn gps_line(snapshot: &Snapshot) -> (Severity, String) {
    let Some(gps) = &snapshot.gps else {
        // No receiver was ever configured, so the chain's other tiers are the whole
        // story. Both are red: a pinned position is not wardriving data.
        return match snapshot.position.source {
            PositionSource::Static => (Severity::Error, "gps: pinned".to_owned()),
            _ => (Severity::Error, "gps: none".to_owned()),
        };
    };

    // Checked first: the chain answering from the GPS is the only thing that makes
    // this line green, and it stays true for `max_age` after a receiver stops talking.
    if snapshot.position.source == PositionSource::Gps {
        let sats = match gps.status {
            GpsStatus::Fixed { satellites: Some(n) } => format!(", {n} sats"),
            _ => String::new(),
        };
        return (Severity::Ok, format!("gps ok{sats}"));
    }

    match &gps.status {
        // Nothing is coming, however hard the thread keeps retrying.
        GpsStatus::NoReceiver => (Severity::Error, "gps: no receiver".to_owned()),
        GpsStatus::Failed(_) => (Severity::Error, "gps: unreadable".to_owned()),
        // Attached and trying. `Fixed` lands here once the chain has fallen through
        // to a staler tier, which is a receiver that had a fix and has lost it.
        GpsStatus::Connecting => (Severity::Warn, "gps connecting".to_owned()),
        GpsStatus::Scanning { .. } => (Severity::Warn, "gps scanning".to_owned()),
        GpsStatus::Searching => (Severity::Warn, "gps searching".to_owned()),
        GpsStatus::Fixed { .. } => (Severity::Warn, "gps fix is stale".to_owned()),
    }
}

/// How many nodes are heartbeating, and how many of those can be driven.
///
/// Both numbers, because a fleet past the bridge's twenty peer slots is entirely
/// alive and entirely undrivable, and one figure leaves that unsayable.
fn nodes_line(snapshot: &Snapshot) -> (Severity, String) {
    if !snapshot.link_up {
        return (Severity::Error, "link down".to_owned());
    }
    match (snapshot.alive, snapshot.assignable) {
        (0, _) => (Severity::Error, "no nodes alive".to_owned()),
        // Drivable first, so it reads as "nine of twenty" — the plain-English order,
        // and the one that puts the number an operator can act on at the front.
        (alive, drivable) if alive > drivable => {
            (Severity::Warn, format!("nodes {drivable} of {alive}"))
        }
        (alive, _) => (Severity::Ok, format!("nodes {alive}")),
    }
}

/// Distinct Wi-Fi access points, estimated.
fn aps_line(snapshot: &Snapshot) -> (Severity, String) {
    (Severity::Ok, format!("APs ~{}", approx(snapshot.unique_wifi_aps)))
}

/// Distinct BLE addresses, estimated.
fn ble_line(snapshot: &Snapshot) -> (Severity, String) {
    (Severity::Ok, format!("BLE ~{}", approx(snapshot.unique_ble_aps)))
}

/// How well the bridge is hearing the fleet.
fn rssi_line(snapshot: &Snapshot) -> (Severity, String) {
    let heard: Vec<i8> =
        snapshot.nodes.iter().filter(|n| n.alive).filter_map(|n| n.state.link_rssi).collect();

    let Some(&min) = heard.iter().min() else {
        // Say which of the two silences this is rather than printing a zero. With
        // nothing alive the line above has already said so, and a second fault
        // colour for one fact reads as two.
        return if snapshot.alive == 0 {
            (Severity::Ok, "rssi —".to_owned())
        } else {
            (Severity::Error, "rssi: none heard".to_owned())
        };
    };

    // Floored rather than truncated, which for a negative dBm is the pessimistic
    // direction: `/` rounds towards zero, so a fleet at -65 and -66 would average to
    // -65 and sit exactly on the right side of a threshold set to catch it.
    let total: i32 = heard.iter().map(|&r| i32::from(r)).sum();
    let mean = total.div_euclid(heard.len() as i32);

    let weak = mean < i32::from(RSSI_WEAK_AVG) || min < RSSI_WEAK_MIN;
    let level = if weak { Severity::Warn } else { Severity::Ok };
    // Two shapes, because a subject, two figures and two labels do not fit seventeen
    // columns together. When the figures differ the labels earn the room — an
    // unlabelled pair is an order the reader has to have been told, and this is a line
    // meant to be read at a glance rather than learnt. When they agree there is one
    // number, no order to describe, and the subject takes the room back. A fleet the
    // bridge hears equally well is the ordinary case on a bench, and one node is
    // always this case.
    let text = if mean == i32::from(min) {
        format!("rssi {}", dbm(mean))
    } else {
        format!("avg {} min {}", dbm(mean), dbm(i32::from(min)))
    };
    (level, text)
}

/// One RSSI reading, in the three characters the line can spare for it.
///
/// [`RSSI_FLOOR`] and below is `BAD` rather than the figure. Three digits and a sign is
/// one character more than the panel has, and the exact number stopped meaning anything
/// well above it: a link this weak is not one that is nearly working, and what an
/// operator does about -104 dBm is what they do about -100. The word is the width of an
/// ordinary reading, so the line does not change shape as a node crosses.
fn dbm(value: i32) -> String {
    if value <= RSSI_FLOOR { "BAD".to_owned() } else { value.to_string() }
}

/// A count rendered short enough for a narrow line.
///
/// Three significant figures, because these are estimates: [`crate::distinct`] puts
/// the standard error at about 0.8%, so the digits past three are noise once there
/// are enough of them to matter. Shared with the view rather than written twice —
/// the panel and the terminal reporting different numbers for one estimate is a bug
/// nobody would think to look for.
#[must_use]
pub fn approx(n: u64) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let (k, m) = (n as f64 / 1e3, n as f64 / 1e6);
    // Each unit is judged on the figure as it would be printed, not the raw count, so one
    // that rounds up past its unit's range (99,950 to "100.0k") moves on to the next.
    for (value, digits, below, unit) in [(k, 1, 100.0, "k"), (k, 0, 1000.0, "k"), (m, 2, 10.0, "M")]
    {
        let text = format!("{value:.digits$}");
        if text.parse::<f64>().is_ok_and(|shown| shown < below) {
            return format!("{text}{unit}");
        }
    }
    format!("{m:.1}M")
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use wartui_proto::air::Capabilities;
    use wartui_proto::plan::ChannelPool;

    use super::*;
    use crate::engine::{Counters, NodeState, NodeView, Now, StoreStats};
    use crate::gps::{GpsCounters, GpsView};
    use crate::position::Fix;

    const EPOCH_MS: i64 = 1_777_642_477_000;

    /// The geometry the T-Dongle-C5's panel actually reports.
    const SCREEN: Panel = Panel { cols: 17, rows: 5 };

    /// A capture with nothing attached and nothing heard, which is what the first
    /// second of every run looks like.
    fn quiet() -> Snapshot {
        Snapshot {
            bridge: None,
            link_up: true,
            link_error: None,
            pool: ChannelPool::Us,
            plan: None,
            ble_node: None,
            nodes: Vec::new(),
            alive: 0,
            assignable: 0,
            tail: Vec::new(),
            unique_wifi_aps: 0,
            unique_ble_aps: 0,
            counters: Counters::default(),
            store: StoreStats::default(),
            bridge_status: None,
            started_at_ms: EPOCH_MS,
            now_ms: EPOCH_MS,
            position: Fix::none(),
            gps: None,
        }
    }

    fn node(last: u8, rssi: Option<i8>) -> NodeView {
        let now = Now { mono: Instant::now(), unix_ms: EPOCH_MS };
        let mut state = NodeState::new([0x02, 0x00, 0x5E, 0x10, 0x57, last], now);
        state.last_heartbeat = Some(now.mono);
        state.capabilities = Some(Capabilities::here(true, true));
        state.link_rssi = rssi;
        NodeView { state, alive: true, assignable: true }
    }

    /// A fleet of `rssi.len()` nodes, every one of them heartbeating.
    fn fleet(rssi: &[Option<i8>]) -> Snapshot {
        let mut snapshot = quiet();
        snapshot.nodes = rssi.iter().enumerate().map(|(n, r)| node(n as u8, *r)).collect();
        snapshot.alive = snapshot.nodes.len();
        snapshot.assignable = snapshot.nodes.len();
        snapshot
    }

    fn gps(status: GpsStatus, source: PositionSource) -> Snapshot {
        let mut snapshot = quiet();
        snapshot.position = Fix { source, ..Fix::none() };
        snapshot.gps = Some(GpsView {
            status,
            counters: GpsCounters::default(),
            last_fix_ms: None,
            settled: None,
            pinned_baud: false,
        });
        snapshot
    }

    /// The text of the line at `row`, so a test can read what an operator would.
    fn row(snapshot: &Snapshot, n: usize) -> (Severity, String) {
        let lines = render(snapshot, SCREEN);
        let line = &lines[n];
        (line.level, line.text.as_str().to_owned())
    }

    #[test]
    fn a_capture_that_has_heard_nothing_still_fills_every_line() {
        let lines = render(&quiet(), SCREEN);
        assert_eq!(lines.len(), 5);
        // Not a blank screen and not a zero pretending to be a measurement: each
        // line says which kind of nothing this is.
        assert_eq!(lines[0].text.as_str(), "gps: none");
        assert_eq!(lines[1].text.as_str(), "no nodes alive");
        assert_eq!(lines[4].text.as_str(), "rssi —");
    }

    #[test]
    fn the_gps_line_is_green_only_on_a_live_fix() {
        // The chain answering from the receiver is the whole of the rule, and it is
        // what the satellite count hangs off too.
        let (level, text) =
            row(&gps(GpsStatus::Fixed { satellites: Some(9) }, PositionSource::Gps), 0);
        assert_eq!(level, Severity::Ok);
        assert_eq!(text, "gps ok, 9 sats");

        // A receiver that has not said how many, which some do not.
        let (level, text) =
            row(&gps(GpsStatus::Fixed { satellites: None }, PositionSource::Gps), 0);
        assert_eq!(level, Severity::Ok);
        assert_eq!(text, "gps ok");
    }

    #[test]
    fn a_receiver_that_is_still_trying_is_yellow_rather_than_red() {
        for status in [
            GpsStatus::Connecting,
            GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 9_600 },
            GpsStatus::Searching,
            // Had a fix, lost it, and the chain has fallen through to a staler tier.
            // Indoors, in other words — which is worth noticing and is not a fault.
            GpsStatus::Fixed { satellites: Some(3) },
        ] {
            let (level, _) = row(&gps(status.clone(), PositionSource::None), 0);
            assert_eq!(level, Severity::Warn, "{status:?} is a receiver still trying");
        }
    }

    #[test]
    fn nothing_that_can_produce_another_fix_is_red() {
        for status in [GpsStatus::NoReceiver, GpsStatus::Failed("no such port".to_owned())] {
            let (level, _) = row(&gps(status.clone(), PositionSource::None), 0);
            assert_eq!(level, Severity::Error, "{status:?} is not going to answer");
        }

        // A pinned `--lat`/`--lon` is red however deliberate it was: the capture is
        // being written against a constant position, which is the thing to notice.
        let mut pinned = quiet();
        pinned.position = Fix { source: PositionSource::Static, ..Fix::none() };
        let (level, text) = row(&pinned, 0);
        assert_eq!(level, Severity::Error);
        assert_eq!(text, "gps: pinned");
    }

    #[test]
    fn a_fleet_that_cannot_all_be_driven_says_both_numbers() {
        let mut snapshot = fleet(&[Some(-40), Some(-45)]);
        snapshot.alive = 5;
        snapshot.assignable = 2;
        let (level, text) = row(&snapshot, 1);
        assert_eq!(level, Severity::Warn);
        assert_eq!(text, "nodes 2 of 5");

        // Nothing to say twice when every alive node is drivable.
        let (level, text) = row(&fleet(&[Some(-40), Some(-45)]), 1);
        assert_eq!(level, Severity::Ok);
        assert_eq!(text, "nodes 2");
    }

    #[test]
    fn a_link_that_is_down_outranks_whatever_the_fleet_last_looked_like() {
        let mut snapshot = fleet(&[Some(-40)]);
        snapshot.link_up = false;
        let (level, text) = row(&snapshot, 1);
        assert_eq!(level, Severity::Error);
        assert_eq!(text, "link down");
    }

    #[test]
    fn the_counts_are_rendered_as_the_estimates_they_are() {
        let mut snapshot = quiet();
        snapshot.unique_wifi_aps = 12_345;
        snapshot.unique_ble_aps = 42;
        assert_eq!(row(&snapshot, 2), (Severity::Ok, "APs ~12.3k".to_owned()));
        assert_eq!(row(&snapshot, 3), (Severity::Ok, "BLE ~42".to_owned()));
    }

    #[test]
    fn a_number_that_is_both_min_and_average_is_said_once() {
        // One node is the obvious case.
        let (level, text) = row(&fleet(&[Some(-52)]), 4);
        assert_eq!(level, Severity::Ok);
        assert_eq!(text, "rssi -52");

        // So is a fleet the bridge hears equally well, which is what the simulator
        // produces and what a bench of nodes on one desk looks like.
        let (_, text) = row(&fleet(&[Some(-55), Some(-55), Some(-55)]), 4);
        assert_eq!(text, "rssi -55");
    }

    #[test]
    fn a_weak_average_and_a_single_weak_node_are_both_worth_a_warning() {
        // Every node weak: the average carries it.
        let (level, _) = row(&fleet(&[Some(-68), Some(-67)]), 4);
        assert_eq!(level, Severity::Warn);

        // One node at the edge of a fleet whose average is comfortable, which is
        // exactly the case `RSSI_WEAK_MIN` exists for.
        let (level, text) = row(&fleet(&[Some(-40), Some(-40), Some(-72)]), 4);
        assert_eq!(level, Severity::Warn);
        assert_eq!(text, "avg -51 min -72");

        let (level, _) = row(&fleet(&[Some(-40), Some(-45)]), 4);
        assert_eq!(level, Severity::Ok);

        // A fleet astride the threshold. The true average is -65.5, and the warning is
        // the point of the line, so the half goes to the fleet's detriment rather than
        // rounding towards zero into a green line on a fleet this constant exists to
        // catch.
        let (level, text) = row(&fleet(&[Some(-65), Some(-66)]), 4);
        assert_eq!(level, Severity::Warn);
        // And the floored average has met the minimum, so the line collapses to one
        // figure — which is the shape, not a single node.
        assert_eq!(text, "rssi -66");
    }

    #[test]
    fn nodes_the_bridge_has_not_measured_are_skipped_rather_than_counted_as_zero() {
        // A node heard only through its observations has no link RSSI yet. Folding
        // its absence in as a nought would read as a perfect link.
        let (level, text) = row(&fleet(&[Some(-60), None]), 4);
        assert_eq!(level, Severity::Ok);
        assert_eq!(text, "rssi -60");

        // None of them measured, with nodes alive: a fault rather than a silence.
        let (level, text) = row(&fleet(&[None, None]), 4);
        assert_eq!(level, Severity::Error);
        assert_eq!(text, "rssi: none heard");
    }

    #[test]
    fn a_node_outside_the_topology_timeout_is_not_folded_in() {
        // Its last measurement is real and long past; the fleet's link health is
        // the fleet that is still here.
        let mut snapshot = fleet(&[Some(-40), Some(-90)]);
        snapshot.nodes[1].alive = false;
        snapshot.alive = 1;
        snapshot.assignable = 1;
        let (level, text) = row(&snapshot, 4);
        assert_eq!(level, Severity::Ok);
        assert_eq!(text, "rssi -40");
    }

    #[test]
    fn a_reading_too_weak_to_print_is_named_rather_than_numbered() {
        // Three digits and a sign is a character more than the line has, and the exact
        // figure stopped meaning anything long before this: what an operator does about
        // -104 dBm is what they do about -100.
        let (level, text) = row(&fleet(&[Some(-40), Some(-104)]), 4);
        assert_eq!(level, Severity::Warn);
        assert_eq!(text, "avg -72 min BAD");

        // The average can cross on its own, with no single node past the floor.
        let (_, text) = row(&fleet(&[Some(-99), Some(-103)]), 4);
        assert_eq!(text, "avg BAD min BAD");

        // And the boundary is the floor itself, not one past it.
        let (_, text) = row(&fleet(&[Some(-40), Some(-99)]), 4);
        assert_eq!(text, "avg -70 min -99");
        let (_, text) = row(&fleet(&[Some(-42), Some(-100)]), 4);
        assert_eq!(text, "avg -71 min BAD");
    }

    #[test]
    fn nothing_this_composes_overflows_the_panel_that_ships() {
        // `firmware/bridge/src/panel.rs` gets seventeen columns out of its font, and the
        // RSSI line fills every one of them. Rendered wide and measured, so a reworded
        // line that would arrive truncated on the bench fails here instead — the
        // truncation in `fit` is a backstop for an unfamiliar screen, not a licence to
        // write past this one.
        let wide = Panel { cols: 32, rows: 8 };
        let mut worst = fleet(&[Some(-100), Some(-40), None]);
        worst.alive = 20;
        worst.assignable = 9;
        worst.unique_wifi_aps = 9_999_999;
        worst.unique_ble_aps = 9_999_999;
        worst.gps = Some(GpsView {
            status: GpsStatus::Fixed { satellites: Some(24) },
            counters: GpsCounters::default(),
            last_fix_ms: None,
            settled: None,
            pinned_baud: false,
        });
        worst.position = Fix { source: PositionSource::Gps, ..Fix::none() };

        for snapshot in [&worst, &quiet(), &gps(GpsStatus::NoReceiver, PositionSource::Static)] {
            for line in &render(snapshot, wide) {
                assert!(
                    line.text.len() <= usize::from(SCREEN.cols),
                    "{:?} is {} characters, past the panel's {}",
                    line.text,
                    line.text.len(),
                    SCREEN.cols
                );
            }
        }
    }

    #[test]
    fn a_narrower_screen_truncates_rather_than_overflowing() {
        let narrow = Panel { cols: 8, rows: 8 };
        let lines = render(&fleet(&[Some(-40), Some(-72)]), narrow);
        for line in &lines {
            assert!(line.text.len() <= 8, "{:?} is wider than the panel", line.text);
        }
        assert_eq!(lines[4].text.as_str(), "avg -56 ");
    }

    #[test]
    fn a_shorter_screen_keeps_the_lines_that_fit_from_the_top() {
        let short = Panel { cols: 26, rows: 2 };
        let lines = render(&quiet(), short);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text.as_str(), "gps: none");
        assert_eq!(lines[1].text.as_str(), "no nodes alive");
    }

    #[test]
    fn counts_are_shortened_the_same_way_the_view_shortens_them() {
        assert_eq!(approx(0), "0");
        assert_eq!(approx(9_999), "9999");
        assert_eq!(approx(12_345), "12.3k");
        assert_eq!(approx(506_360), "506k");
        assert_eq!(approx(2_350_000), "2.35M");
        assert_eq!(approx(23_500_000), "23.5M");
        // Each unit judged on the printed figure, so these move up rather than
        // reading as "100.0k" and "1000k".
        assert_eq!(approx(99_950), "100k");
        assert_eq!(approx(999_500), "1.00M");
        assert_eq!(approx(9_999_999), "10.0M");
    }
}
