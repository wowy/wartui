//! The bridge's panel, composed here.
//!
//! A bridge with a screen is sent finished lines and blits them. It composes nothing, parses no air
//! frame, and owns only the three colours a [`Severity`] maps to. The same division keeps the
//! planner on the host: what is likely wrong lives where `cargo test` reaches and a fix costs a
//! `cargo run`, and the board keeps only what is small enough to finish.
//!
//! So this module is a pure function of one [`Snapshot`] and the bridge's announced geometry. No
//! clock: the snapshot carries the times that matter, and liveness comes from the engine in
//! [`crate::engine::NodeView`]. No I/O, and [`crate::runtime`] decides *when* a panel is pushed.
//!
//! Five of the panel's eight lines, leaving room to add one. Each is coloured by its own state, so
//! the panel reads at a glance.
//!
//! # The GPS line is green only on a live, current fix
//!
//! Stricter than the view, on purpose. [`crate::gps`] holds that a receiver nobody attached is not
//! a *fault*, but this line grades the capture, not the subsystem, and a capture written against a
//! constant position is what an operator most needs to see from across a car. So only
//! [`PositionSource::Gps`] is green.
//!
//! A receiver attached and trying (connecting, scanning, searching, or holding a fix the chain
//! stopped believing) is yellow. Nothing attached, a port that will not read, and a pinned
//! `--lat`/`--lon` are red however deliberate, since none will produce another fix.
//!
//! # The RSSI line is link health, not signal strength
//!
//! [`crate::engine::NodeState::link_rssi`] is how strongly *the bridge* heard a node, not how
//! strongly a node heard an access point. It says whether an assignment will land, which earns it
//! one of five lines.

use wartui_proto::link::{Panel, PanelLine, PanelLines, Severity, ShortStr};

use crate::engine::Snapshot;
use crate::gps::GpsStatus;
use crate::position::PositionSource;

/// Mean link RSSI, in dBm, below which the fleet is worth looking at.
///
/// Not a stock link budget. Every radio defaults to 2 dBm
/// ([`wartui_proto::plan::DEFAULT_TX_POWER_QUARTER_DBM`]), and ESP-NOW goes out at 802.11g 24 Mbps,
/// rated at about −74 dBm sensitivity. That is the floor, and a threshold there would warn only as
/// nodes started missing admin windows. These sit above it while the operator can still act: close
/// a window, lift the dongle off the floor, walk a node back.
pub const RSSI_WEAK_AVG: i8 = -65;

/// The reading below which a figure stops being worth printing. Well under both thresholds:
/// [`RSSI_WEAK_MIN`] judges the link, and this judges the number ([`dbm`]).
const RSSI_FLOOR: i32 = -100;

/// Weakest link RSSI, in dBm, below which one node is worth looking at. More negative than
/// [`RSSI_WEAK_AVG`], which says where both come from, because one edge node in a fine fleet is
/// what an average cannot show.
pub const RSSI_WEAK_MIN: i8 = -70;

/// Lay out a panel for one snapshot. Lines are cut to `panel.cols` and capped at `panel.rows`, so a
/// smaller screen loses line ends rather than overflowing. The caller sends the result verbatim.
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
        // Capacity is `PANEL_ROWS`, so a bridge claiming more rows than the wire carries gets what
        // fits, not a truncated frame.
        if lines.push(PanelLine { level, text: fit(&text, panel.cols) }).is_err() {
            break;
        }
    }
    lines
}

/// One line's text, cut to the panel's width.
///
/// By characters, not bytes: a slice at a non-boundary byte would panic the first time something
/// non-ASCII slipped in. Unpanicked it would still be wrong: the bridge's font covers U+0020 to
/// U+007F and draws `?` for anything else, which no host test sees.
/// `render_composes_only_ascii` holds the line.
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
        // No receiver configured, so the chain's other tiers are the story. Both red: a pinned
        // position is not wardriving data.
        return match snapshot.position.source {
            PositionSource::Static => (Severity::Error, "gps: pinned".to_owned()),
            _ => (Severity::Error, "gps: none".to_owned()),
        };
    };

    // First: only the chain answering from the GPS makes this green, and that holds for `max_age`
    // after a receiver stops talking.
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
        // Attached and trying. `Fixed` lands here once the chain fell through to a staler tier: a
        // receiver that lost its fix.
        GpsStatus::Connecting => (Severity::Warn, "gps connecting".to_owned()),
        GpsStatus::Scanning { .. } => (Severity::Warn, "gps scanning".to_owned()),
        GpsStatus::Searching => (Severity::Warn, "gps searching".to_owned()),
        GpsStatus::Fixed { .. } => (Severity::Warn, "gps fix is stale".to_owned()),
    }
}

/// How many nodes are heartbeating, and how many can be driven. Both, because a node refused a peer
/// slot, or with no declared radio, is alive but undrivable. With none alive, a session that has
/// seen none is waiting, and one that has seen some has lost them.
fn nodes_line(snapshot: &Snapshot) -> (Severity, String) {
    if !snapshot.link_up {
        return (Severity::Error, "link down".to_owned());
    }
    match (snapshot.alive, snapshot.assignable) {
        (0, _) if snapshot.nodes.is_empty() => (Severity::Warn, "waiting for nodes".to_owned()),
        (0, _) => (Severity::Error, "all nodes silent".to_owned()),
        // Drivable first, "nine of twenty": the plain-English order, with the actionable number in
        // front.
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
        // Say which silence this is rather than print zero. With nothing alive the line above says
        // so, and a second fault colour for one fact reads as two.
        return if snapshot.alive == 0 {
            (Severity::Ok, "rssi n/a".to_owned())
        } else {
            (Severity::Error, "rssi: none heard".to_owned())
        };
    };

    // Floored, the pessimistic direction for negative dBm. `/` rounds towards zero, so -65 and -66
    // would average -65, just the safe side of a threshold meant to catch them.
    let total: i32 = heard.iter().map(|&r| i32::from(r)).sum();
    let mean = total.div_euclid(heard.len() as i32);

    let weak = mean < i32::from(RSSI_WEAK_AVG) || min < RSSI_WEAK_MIN;
    let level = if weak { Severity::Warn } else { Severity::Ok };
    // Two shapes: a subject, two figures and two labels do not fit seventeen columns. When the
    // figures differ, labels earn the room, because an unlabelled pair's order must be learnt. When
    // they agree there is one number, and the subject takes the room back. That is the bench case,
    // and always the one-node case.
    let text = if mean == i32::from(min) {
        format!("rssi {}", dbm(mean))
    } else {
        format!("avg {} min {}", dbm(mean), dbm(i32::from(min)))
    };
    (level, text)
}

/// One RSSI reading, in the three characters the line can spare. [`RSSI_FLOOR`] and below reads
/// `BAD`: three digits and a sign is a character too wide, and the exact figure stopped meaning
/// anything well above it. An operator does the same about -104 dBm as about -100. The word is as
/// wide as a reading, so the line keeps its shape.
fn dbm(value: i32) -> String {
    if value <= RSSI_FLOOR { "BAD".to_owned() } else { value.to_string() }
}

/// A count rendered short for a narrow line. Three significant figures, because these are estimates
/// with about 0.8% standard error ([`crate::distinct`]), so later digits are noise. Shared with the
/// view, since the panel and terminal disagreeing on one estimate is a bug nobody would look for.
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
    use wartui_proto::plan::{ChannelPool, DEFAULT_TX_POWER_QUARTER_DBM};

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
            preferred_ble: None,
            remember_ble: true,
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
            tx_power: DEFAULT_TX_POWER_QUARTER_DBM,
            bridge_tx_power: DEFAULT_TX_POWER_QUARTER_DBM,
        }
    }

    fn node(last: u8, rssi: Option<i8>) -> NodeView {
        let now = Now { mono: Instant::now(), unix_ms: EPOCH_MS };
        let mut state = NodeState::new([0x02, 0x00, 0x5E, 0x10, 0x57, last], now);
        state.last_heartbeat = Some(now.mono);
        state.capabilities = Some(Capabilities::here(true));
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
        snapshot.gps = Some(gps_view(status));
        snapshot
    }

    /// A receiver doing `status`, with nothing else to say.
    fn gps_view(status: GpsStatus) -> GpsView {
        GpsView {
            status,
            counters: GpsCounters::default(),
            last_fix_ms: None,
            settled: None,
            pinned_baud: false,
        }
    }

    /// The text of the line at `row`, so a test can read what an operator would.
    fn row(snapshot: &Snapshot, n: usize) -> (Severity, String) {
        let lines = render(snapshot, SCREEN);
        let line = &lines[n];
        (line.level, line.text.as_str().to_owned())
    }

    #[test]
    fn render_fills_every_line_when_snapshot_empty() {
        let lines = render(&quiet(), SCREEN);
        assert_eq!(lines.len(), 5);
        // Not a blank screen and not a zero pretending to be a measurement: each
        // line says which kind of nothing this is.
        assert_eq!(lines[0].text.as_str(), "gps: none");
        assert_eq!(lines[1].text.as_str(), "waiting for nodes");
        assert_eq!(lines[4].text.as_str(), "rssi n/a");
    }

    #[test]
    fn render_marks_gps_ok_when_fix_live() {
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
    fn render_marks_gps_warn_when_receiver_trying() {
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
    fn render_marks_gps_error_when_receiver_gone_or_position_pinned() {
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
    fn render_shows_drivable_of_alive_when_some_undrivable() {
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
    fn render_marks_nodes_warn_when_none_seen() {
        let (level, text) = row(&quiet(), 1);
        assert_eq!(level, Severity::Warn);
        assert_eq!(text, "waiting for nodes");
    }

    #[test]
    fn render_marks_nodes_error_when_all_silent() {
        let mut snapshot = fleet(&[Some(-50), Some(-60)]);
        for node in &mut snapshot.nodes {
            node.alive = false;
        }
        snapshot.alive = 0;
        snapshot.assignable = 0;
        let (level, text) = row(&snapshot, 1);
        assert_eq!(level, Severity::Error);
        assert_eq!(text, "all nodes silent");
    }

    #[test]
    fn render_shows_link_down_when_link_down() {
        let mut snapshot = fleet(&[Some(-40)]);
        snapshot.link_up = false;
        let (level, text) = row(&snapshot, 1);
        assert_eq!(level, Severity::Error);
        assert_eq!(text, "link down");
    }

    #[test]
    fn render_shows_unique_counts_approximately() {
        let mut snapshot = quiet();
        snapshot.unique_wifi_aps = 12_345;
        snapshot.unique_ble_aps = 42;
        assert_eq!(row(&snapshot, 2), (Severity::Ok, "APs ~12.3k".to_owned()));
        assert_eq!(row(&snapshot, 3), (Severity::Ok, "BLE ~42".to_owned()));
    }

    #[test]
    fn render_shows_one_rssi_when_figures_agree() {
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
    fn render_marks_rssi_warn_when_link_weak() {
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
    fn render_skips_unmeasured_nodes_in_rssi() {
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
    fn render_skips_dead_nodes_in_rssi() {
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
    fn render_shows_bad_when_rssi_below_floor() {
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
    fn render_fits_every_line_on_the_bridge_panel() {
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
        worst.gps = Some(gps_view(GpsStatus::Fixed { satellites: Some(24) }));
        worst.position = Fix { source: PositionSource::Gps, ..Fix::none() };

        let mut silent = fleet(&[Some(-50)]);
        silent.alive = 0;
        silent.assignable = 0;

        for snapshot in
            [&worst, &quiet(), &silent, &gps(GpsStatus::NoReceiver, PositionSource::Static)]
        {
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
    fn render_composes_only_ascii() {
        // The bridge draws with `FONT_9X15`, whose glyph mapping covers U+0020 to
        // U+007F and quietly substitutes `?` for anything else. So a tidy typographic
        // dash composed here does not reach the glass as a dash — it reaches it as
        // `rssi ?`, and every host-side assertion still passes because they compare
        // the string rather than the pixels. Nothing but this test stands between a
        // well-meant character and that.
        let wide = Panel { cols: 32, rows: 8 };
        let mut worst = fleet(&[Some(-100), Some(-40), None]);
        worst.alive = 20;
        worst.assignable = 9;

        let mut cases = vec![quiet(), worst, fleet(&[Some(-55)])];
        for status in [
            GpsStatus::Connecting,
            GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 9_600 },
            GpsStatus::NoReceiver,
            GpsStatus::Searching,
            GpsStatus::Fixed { satellites: Some(24) },
            GpsStatus::Failed("permission denied".to_owned()),
        ] {
            for source in [PositionSource::Gps, PositionSource::Static, PositionSource::None] {
                cases.push(gps(status.clone(), source));
            }
        }

        for snapshot in &cases {
            for line in &render(snapshot, wide) {
                assert!(
                    line.text.is_ascii(),
                    "{:?} would reach the panel with a `?` in it",
                    line.text
                );
            }
        }
    }

    #[test]
    fn render_cuts_lines_to_narrow_screen() {
        let narrow = Panel { cols: 8, rows: 8 };
        let lines = render(&fleet(&[Some(-40), Some(-72)]), narrow);
        for line in &lines {
            assert!(line.text.len() <= 8, "{:?} is wider than the panel", line.text);
        }
        assert_eq!(lines[4].text.as_str(), "avg -56 ");
    }

    #[test]
    fn render_keeps_top_rows_when_screen_short() {
        let short = Panel { cols: 26, rows: 2 };
        let lines = render(&quiet(), short);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text.as_str(), "gps: none");
        assert_eq!(lines[1].text.as_str(), "waiting for nodes");
    }

    #[test]
    fn approx_shortens_to_three_figures() {
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
