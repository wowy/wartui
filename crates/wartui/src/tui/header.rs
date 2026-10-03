//! The header: the bridge and its link, then pool, plan, session time and position.
//!
//! The border is green only while a bridge's link is up and rows have a position, and
//! red otherwise. A glance at it says whether the capture is working.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use wartui_core::engine::Snapshot;
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_core::position::PositionSource;

use super::format::{elapsed, short_mac};

/// Draw the header box into `area`.
pub(super) fn draw_header(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot) {
    let (link, border_color) = if let Some(bridge) = &snapshot.bridge {
        let state = if snapshot.link_up { "up" } else { "down" };
        (
            format!(
                "bridge {} ({:?}), fw v{} — link {state}",
                short_mac(&bridge.mac),
                bridge.chip,
                bridge.fw_version
            ),
            if snapshot.link_up && snapshot.position.is_located() {
                Color::Green
            } else {
                Color::Red
            },
        )
    } else {
        ("waiting for bridge".to_owned(), Color::Red)
    };

    // The link-down reason goes in the fault box, which wraps. Here a narrow
    // terminal would clip it.
    let first = vec![Span::raw(link)];

    let mut second = vec![
        Span::raw(format!("pool {}  ", snapshot.pool)),
        planning(snapshot),
        Span::raw(format!("  session {}  ", elapsed(snapshot.now_ms - snapshot.started_at_ms))),
    ];
    second.extend(position(snapshot));

    let body = vec![Line::from(first), Line::from(second)];
    frame.render_widget(
        Paragraph::new(body).block(
            Block::bordered().title(" wartui ").border_style(border_color).title_style(Color::Gray),
        ),
        area,
    );
}

/// Planner status: `N of M` nodes in the plan, or why there is no plan.
///
/// | Shown | Colour | Meaning |
/// |---|---|---|
/// | `N of M` | green | `N` of the `M` nodes heard are in the plan |
/// | `N alive, none usable` | red | heartbeating, but none assignable |
/// | `waiting for nodes` | yellow | none heard yet, normal at startup |
/// | `all nodes silent` | red | every node heard has stopped heartbeating |
fn planning(snapshot: &Snapshot) -> Span<'static> {
    let (text, color) = match snapshot.plan {
        Some(plan) => (format!("{} of {}", plan.node_count(), snapshot.nodes.len()), Color::Green),
        None if snapshot.alive > 0 => {
            (format!("{} alive, none usable", snapshot.alive), Color::Red)
        }
        None if snapshot.nodes.is_empty() => ("waiting for nodes".to_owned(), Color::Yellow),
        None => ("all nodes silent".to_owned(), Color::Red),
    };
    Span::styled(text, Style::new().fg(color))
}

/// The header's no-position warning, then the receiver's state.
fn position(snapshot: &Snapshot) -> Vec<Span<'static>> {
    let fix = if snapshot.position.is_located() {
        Span::default()
    } else {
        Span::styled(
            "pos none — these observations cannot be uploaded",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let mut spans = vec![fix];
    if let Some(gps) = &snapshot.gps
        && let Some(span) = receiver(gps, snapshot.position.source)
    {
        spans.push(span);
    }
    spans
}

/// The receiver's state: `gps ok` while it supplies positions, otherwise why not.
///
/// A GPS that stops supplying positions is otherwise easy to miss, because rows keep
/// arriving with the fallback position. The note stays up until it is fixed.
///
/// `None` when the search found no receiver, which is the usual case. The search runs
/// by default, so saying so on every capture would be noise.
fn receiver(gps: &GpsView, source: PositionSource) -> Option<Span<'static>> {
    // Checked first: for `max_age` after an unplug, the source is still GPS but the
    // port is dead.
    if let GpsStatus::Failed(reason) = &gps.status {
        return Some(Span::styled(format!("  gps: {reason}"), Style::new().fg(Color::Yellow)));
    }
    if source == PositionSource::Gps {
        let sats = match gps.status {
            GpsStatus::Fixed { satellites: Some(n) } => format!(", {n} sats"),
            _ => String::new(),
        };
        return Some(Span::styled(format!("gps ok{sats}"), Style::new().fg(Color::Green)));
    }
    let text = match &gps.status {
        GpsStatus::Connecting => "  gps connecting".to_owned(),
        // The port and rate show because the operator is watching the search try
        // each rate their receiver might use.
        GpsStatus::Scanning { port, baud } => format!("  gps scanning {port} @{baud}"),
        GpsStatus::Searching => "  gps searching".to_owned(),
        GpsStatus::Fixed { .. } => "  gps fix is stale".to_owned(),
        GpsStatus::Failed(_) => "  gps unreadable".to_owned(),
        GpsStatus::NoReceiver => return None,
    };
    Some(Span::styled(text, Style::new().fg(Color::Yellow)))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use wartui_core::gps::GpsCounters;
    use wartui_proto::plan::{ChannelPool, plan};

    use crate::tui::ui::Ui;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;

    #[test]
    fn view_displays_pos_none_indicator_when_capture_lacks_position_fix() {
        let mut positioned = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        positioned.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        assert!(!positioned.backend().to_string().contains("pos none"));

        let mut without = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        without.draw(|frame| draw(frame, &empty(), &mut Ui::default())).expect("drawing");
        assert!(without.backend().to_string().contains("pos none"));
    }

    #[test]
    fn header_displays_pool_name_when_rendering_active_channel_pool() {
        // The header is the only place the pool is named. It spells each one as the
        // manual does.
        let mut snapshot = busy();
        snapshot.pool = ChannelPool::Us;
        assert!(rendered(&snapshot).contains("pool US"));

        snapshot.pool = ChannelPool::Eu;
        assert!(rendered(&snapshot).contains("pool EU"));

        snapshot.pool = ChannelPool::All;
        assert!(rendered(&snapshot).contains("pool All"));
    }

    #[test]
    fn view_explains_gps_status_when_receiver_is_searching_or_stale() {
        let searching =
            with_gps(GpsStatus::Searching, GpsCounters::default(), PositionSource::Static);
        assert!(rendered(&searching).contains("gps searching"));

        let stale = with_gps(
            GpsStatus::Fixed { satellites: Some(8) },
            GpsCounters::default(),
            PositionSource::Static,
        );
        assert!(rendered(&stale).contains("gps fix is stale"));
    }

    #[test]
    fn view_displays_satellite_count_when_gps_fix_is_active() {
        let fixed = with_gps(
            GpsStatus::Fixed { satellites: Some(8) },
            GpsCounters { sentences: 400, fixes: 200, rejected: 1 },
            PositionSource::Gps,
        );
        let screen = rendered(&fixed);
        assert!(!screen.contains("pos none"), "{screen}");
        assert!(screen.contains("gps ok, 8 sats"), "{screen}");
    }

    #[test]
    fn view_omits_gps_status_when_no_receiver_is_present() {
        // Most captures are static, so a permanent "gps: none" would be noise.
        assert!(!rendered(&busy()).contains("gps"));
    }

    #[test]
    fn view_displays_failure_message_when_receiver_disconnects_mid_capture() {
        // The seconds after the receiver is unplugged. Rows still carry its fix, but
        // the port is gone.
        let unplugged = with_gps(
            GpsStatus::Failed("/dev/cu.gps: Device not configured".to_owned()),
            GpsCounters { sentences: 400, fixes: 200, rejected: 0 },
            PositionSource::Gps,
        );
        let screen = rendered(&unplugged);
        assert!(screen.contains("Device not configured"), "{screen}");
        assert!(!screen.contains("gps ok"), "{screen}");
    }

    #[test]
    fn view_displays_port_and_baud_rate_when_gps_ladder_is_scanning() {
        let scanning = with_gps(
            GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 38_400 },
            GpsCounters::default(),
            PositionSource::Static,
        );
        assert!(rendered(&scanning).contains("gps scanning /dev/ttyUSB0 @38400"));
    }

    #[test]
    fn view_warns_no_drivable_nodes_when_all_nodes_are_refused_peer_slots() {
        // Every node heartbeating, none reachable, so there is nothing to partition.
        let mut snapshot = busy();
        snapshot.plan = None;
        snapshot.nodes = vec![refused(0x21), refused(0x22), refused(0x23)];
        snapshot.alive = 3;
        snapshot.assignable = 0;

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("3 alive, none usable"), "got {rendered}");
        assert!(!rendered.contains("all nodes silent"), "they are all heartbeating");
    }

    #[test]
    fn view_displays_planner_status_when_rendering_header() {
        let mut empty = busy();
        empty.nodes.clear();
        empty.alive = 0;
        empty.assignable = 0;
        let mut waiting = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        waiting.draw(|frame| draw(frame, &empty, &mut Ui::default())).expect("drawing");
        assert!(waiting.backend().to_string().contains("waiting for nodes"));

        let mut snapshot = busy();
        snapshot.plan = plan(ChannelPool::Us, 4);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        assert!(terminal.backend().to_string().contains("4 of 5"));

        // A lone node holds the whole pool. The header says nothing extra about it.
        let mut lone = busy();
        lone.plan = plan(ChannelPool::Us, 1);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &lone, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("1 of 5"));
        assert!(!rendered.contains("rotating"));
    }

    /// The colour of the header's top-left border corner.
    fn header_border(snapshot: &Snapshot) -> Option<Color> {
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, &mut Ui::default())).expect("drawing");
        terminal.backend().buffer()[(0, 0)].style().fg
    }

    #[test]
    fn header_border_is_red_when_link_is_down_with_position_held() {
        let mut snapshot = busy();
        assert!(snapshot.bridge.is_some() && snapshot.position.is_located());
        assert_eq!(header_border(&snapshot), Some(Color::Green), "the baseline");

        snapshot.link_up = false;
        assert_eq!(header_border(&snapshot), Some(Color::Red));
    }

    #[test]
    fn planning_reports_waiting_in_yellow_when_no_node_has_been_seen() {
        let mut snapshot = busy();
        snapshot.plan = None;
        snapshot.nodes.clear();
        snapshot.alive = 0;
        snapshot.assignable = 0;
        let span = planning(&snapshot);
        assert_eq!(span.content, "waiting for nodes");
        assert_eq!(span.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn planning_reports_silence_in_red_when_seen_nodes_stop_heartbeating() {
        let mut snapshot = busy();
        snapshot.plan = None;
        assert!(!snapshot.nodes.is_empty());
        snapshot.alive = 0;
        snapshot.assignable = 0;
        let span = planning(&snapshot);
        assert_eq!(span.content, "all nodes silent");
        assert_eq!(span.style.fg, Some(Color::Red));
    }
}
