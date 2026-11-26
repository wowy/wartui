//! The border is green only while a bridge's link is up and rows have a position, and
//! red otherwise. A glance at it says whether the capture is working.
//!
//! The first line names the host link the run uses, `host usb` or `host bluetooth`, as `wartui
//! status` names it: `bluetooth` alone is the node that scans Bluetooth. Over Bluetooth it adds
//! the link's MTU from the bridge's latest `Status`, since less than the C6's 182 is a slower
//! link. It does not say the link is encrypted and bonded: the bridge serves no other, so a
//! `Status` that arrived says so. Over USB it adds a bonded Bluetooth host connected to the C6,
//! which waits while USB holds the bridge: that is why a phone goes unserved.
//!
//! The line is not wrapped, so it stays short of an 80-column terminal's 78, fan reading included.
//!
//! The second line starts with what the bridge's planner scans, because the bridge decides it and
//! keeps it with no host attached: the run asks for it, and only the bridge's report says it took.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use wartui_core::engine::Snapshot;
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_core::position::PositionSource;
use wartui_proto::fleet::ScanMode;
use wartui_proto::mac;

use super::format::elapsed;
use crate::config::Link;

/// Draw the header box into `area`.
pub(super) fn draw_header(frame: &mut Frame<'_>, area: Rect, snapshot: &Snapshot, over: Link) {
    let (link, border_color) = if let Some(bridge) = &snapshot.bridge {
        let state = if snapshot.link_up { "up" } else { "down" };
        (
            format!(
                "bridge {}, fw v{} — {}, link {state}",
                mac::short(&bridge.mac),
                bridge.fw_version,
                host(snapshot, over)
            ),
            if snapshot.link_up && snapshot.position.is_located() {
                Color::Green
            } else {
                Color::Red
            },
        )
    } else {
        (format!("waiting for bridge — {}", host(snapshot, over)), Color::Red)
    };

    // The link-down reason goes in the fault box, which wraps. Here a narrow
    // terminal would clip it.
    let mut first = vec![Span::raw(link)];
    if let Some(fan) = fan(snapshot) {
        first.push(Span::raw(format!("  {fan}")));
    }

    // No pool before the bridge's first `Status`, and a placeholder for the scan.
    let mut second: Vec<Span> =
        snapshot.pool.map(|pool| Span::raw(format!("pool {pool}  "))).into_iter().collect();
    second.extend(scan(snapshot));
    second.push(Span::raw("  "));
    second.extend([
        planning(snapshot),
        Span::raw(format!("  session {}  ", elapsed(snapshot.now_ms - snapshot.started_at_ms))),
    ]);
    second.extend(position(snapshot));

    let body = vec![Line::from(first), Line::from(second)];
    frame.render_widget(
        Paragraph::new(body).block(
            Block::bordered().title(" wartui ").border_style(border_color).title_style(Color::Gray),
        ),
        area,
    );
}

/// The host link the run uses, and what the bridge's latest `Status` says of it. The status is
/// read only while the link is up, since a stale one would describe a host that has gone.
///
/// | Shown | Meaning |
/// |---|---|
/// | `host usb` | the run reaches the bridge over USB |
/// | `host usb; Bluetooth waits` | a host the bridge would serve is connected to the C6, and USB holds the bridge |
/// | `host bluetooth` | over Bluetooth, before the bridge has reported the link |
/// | `host bluetooth, MTU 182` | the link as the bridge last reported it |
///
/// A central the bridge would not serve, one with no bond or no encryption, is not waiting for
/// USB: the C6 drops it. `wartui status` names it.
fn host(snapshot: &Snapshot, link: Link) -> String {
    let central = snapshot
        .bridge_status
        .filter(|_| snapshot.link_up)
        .and_then(|status| status.bluetooth.central);
    match (link, central) {
        (Link::Usb, Some(central)) if central.encrypted && central.bonded => {
            "host usb; Bluetooth waits".to_owned()
        }
        (Link::Usb, _) => "host usb".to_owned(),
        (Link::Bluetooth, None) => "host bluetooth".to_owned(),
        (Link::Bluetooth, Some(central)) => format!("host bluetooth, MTU {}", central.mtu),
    }
}

/// The carrier's lid fan as the bridge last reported it: `fan 40% 1830 rpm`, or `fan 40%` with no
/// tach reading. `None` with no carrier, before the bridge's first `Status`, and with the link
/// down, when that report is stale.
fn fan(snapshot: &Snapshot) -> Option<String> {
    if !snapshot.link_up {
        return None;
    }
    let fan = snapshot.bridge_status?.fan?;
    Some(match fan.rpm {
        Some(rpm) => format!("fan {} {rpm} rpm", fan.duty),
        None => format!("fan {}", fan.duty),
    })
}

/// What the bridge's planner scans.
///
/// | Shown | Color | Meaning |
/// |---|---|---|
/// | `scan —` | gray | the bridge has not reported yet |
/// | `scan stopped` | yellow | every node idles; the run starts the scan when the bridge answers |
/// | `scan wifi` | plain | every node sniffs Wi-Fi |
/// | `scan wifi +bt N7` | plain | and the bridge gave N7 the Bluetooth scan |
/// | `scan wifi +bt waiting for a second node` | yellow hint | Bluetooth asked for, no node picked |
fn scan(snapshot: &Snapshot) -> Vec<Span<'static>> {
    let Some(mode) = snapshot.scan else {
        return vec![Span::styled("scan —", Style::new().fg(Color::DarkGray))];
    };
    match (mode, snapshot.ble_slot) {
        (ScanMode::Stopped, _) => {
            vec![Span::styled("scan stopped", Style::new().fg(Color::Yellow))]
        }
        (ScanMode::Wifi, _) => vec![Span::raw("scan wifi")],
        (ScanMode::WifiBluetooth, Some(slot)) => {
            vec![Span::raw(format!("scan wifi +bt {}", snapshot.node_board().label(slot)))]
        }
        // The bridge keeps a lone node on Wi-Fi, so Bluetooth waits for a second to join.
        (ScanMode::WifiBluetooth, None) => vec![
            Span::raw("scan wifi +bt "),
            Span::styled(SECOND_NODE, Style::new().fg(Color::Yellow)),
        ],
    }
}

/// The header's hint while Bluetooth is asked for and no node holds it.
const SECOND_NODE: &str = "waiting for a second node";

/// Planner status: `N of M` nodes holding a share of the bridge's plan, or why none does.
///
/// | Shown | Color | Meaning |
/// |---|---|---|
/// | `N of M` | green | `N` of the `M` nodes heard hold a share |
/// | `N alive, none usable` | red | heartbeating, but none assignable |
/// | `waiting for nodes` | yellow | none heard yet, normal at startup |
/// | `2 need update` | red | heartbeating on another major or minor, nothing drivable |
/// | `all nodes silent` | red | every node heard has stopped heartbeating |
fn planning(snapshot: &Snapshot) -> Span<'static> {
    let shares = snapshot.nodes.iter().filter(|node| node.share.is_some()).count();
    let (text, color) = match shares {
        1.. => (format!("{shares} of {}", snapshot.nodes.len()), Color::Green),
        0 if snapshot.alive > 0 => (format!("{} alive, none usable", snapshot.alive), Color::Red),
        0 if snapshot.nodes.is_empty() => ("waiting for nodes".to_owned(), Color::Yellow),
        0 if let Some(text) = snapshot.undrivable_text() => (text, Color::Red),
        0 => ("all nodes silent".to_owned(), Color::Red),
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
    if let Some(gps) = &snapshot.gps {
        spans.push(receiver(gps, snapshot.position.source));
    }
    spans
}

/// The receiver's state: `gps ok` while it supplies positions, otherwise why not.
///
/// A GPS that stops supplying positions is otherwise easy to miss, because rows keep
/// arriving with the fallback position. The note stays up until it is fixed.
///
/// `gps no receiver` when neither the host nor the bridge has one. Every P4 reads a GPS port,
/// so its absence is worth a word.
fn receiver(gps: &GpsView, source: PositionSource) -> Span<'static> {
    // Checked first: for `max_age` after an unplug, the source is still GPS but the
    // port is dead.
    if let GpsStatus::Failed(reason) = &gps.status {
        return Span::styled(format!("  gps: {reason}"), Style::new().fg(Color::Yellow));
    }
    if source == PositionSource::Gps {
        let sats = match gps.status {
            GpsStatus::Fixed { satellites: Some(n) } => format!(", {n} sats"),
            _ => String::new(),
        };
        return Span::styled(format!("gps ok{sats}"), Style::new().fg(Color::Green));
    }
    let text = match &gps.status {
        GpsStatus::Connecting => "  gps connecting".to_owned(),
        // The port and rate show because the operator is watching the search try
        // each rate their receiver might use.
        GpsStatus::Scanning { port, baud } => format!("  gps scanning {port} @{baud}"),
        GpsStatus::Searching => "  gps searching".to_owned(),
        GpsStatus::Fixed { .. } => "  gps fix is stale".to_owned(),
        GpsStatus::Failed(_) => "  gps unreadable".to_owned(),
        GpsStatus::NoReceiver => "  gps no receiver".to_owned(),
    };
    Span::styled(text, Style::new().fg(Color::Yellow))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use wartui_app::settings::Settings;
    use wartui_core::gps::GpsCounters;
    use wartui_proto::ble::ATT_MTU;
    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::fan::{FanDuty, FanStats};
    use wartui_proto::link::{BluetoothStatus, Central, HostOwner};
    use wartui_proto::plan::ChannelPool;

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;

    #[test]
    fn view_displays_pos_none_indicator_when_capture_lacks_position_fix() {
        let mut positioned = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        positioned.draw(|frame| draw(frame, &busy(), &mut idle())).expect("drawing");
        assert!(!positioned.backend().to_string().contains("pos none"));

        let mut without = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        without.draw(|frame| draw(frame, &empty(), &mut idle())).expect("drawing");
        assert!(without.backend().to_string().contains("pos none"));
    }

    #[test]
    fn header_displays_pool_name_when_rendering_active_channel_pool() {
        // The header is the only place the pool is named. It spells each one as the
        // manual does.
        let mut snapshot = busy();
        snapshot.pool = Some(ChannelPool::Us);
        assert!(rendered(&snapshot).contains("pool US"));

        snapshot.pool = Some(ChannelPool::Eu);
        assert!(rendered(&snapshot).contains("pool EU"));

        snapshot.pool = Some(ChannelPool::All);
        assert!(rendered(&snapshot).contains("pool All"));
    }

    #[test]
    fn header_omits_pool_when_bridge_has_reported_none() {
        let mut snapshot = busy();
        snapshot.pool = None;
        assert!(!rendered(&snapshot).contains("pool"), "{}", rendered(&snapshot));
    }

    /// The text of the header's scan spans, and the color of the last.
    fn scan_shown(snapshot: &Snapshot) -> (String, Option<Color>) {
        let spans = scan(snapshot);
        let text = spans.iter().map(|span| span.content.as_ref()).collect();
        (text, spans.last().and_then(|span| span.style.fg))
    }

    #[test]
    fn header_shows_scan_wifi_when_bridge_scans_wifi() {
        let snapshot = Snapshot { scan: Some(ScanMode::Wifi), ..busy() };
        assert_eq!(scan_shown(&snapshot), ("scan wifi".to_owned(), None));
        let screen = rendered(&snapshot);
        assert!(screen.contains("pool All  scan wifi  "), "{screen}");
        assert!(!screen.contains("+bt"), "{screen}");
    }

    #[test]
    fn header_shows_picked_node_by_slot_label_when_bridge_gives_one_bluetooth() {
        let mut snapshot = on_board(NodeBoard::Carrier14);
        snapshot.scan = Some(ScanMode::WifiBluetooth);
        snapshot.ble_slot = Slot::from_u8(6);
        assert_eq!(scan_shown(&snapshot), ("scan wifi +bt N7".to_owned(), None));
        assert!(rendered(&snapshot).contains("scan wifi +bt N7"), "{}", rendered(&snapshot));
        assert!(!rendered(&snapshot).contains(SECOND_NODE), "{}", rendered(&snapshot));
    }

    #[test]
    fn header_hints_at_second_node_when_bluetooth_is_wanted_and_no_node_is_picked() {
        let snapshot = Snapshot { scan: Some(ScanMode::WifiBluetooth), ble_slot: None, ..busy() };
        let shown = scan_shown(&snapshot);
        assert_eq!(
            shown,
            ("scan wifi +bt waiting for a second node".to_owned(), Some(Color::Yellow))
        );
        let screen = rendered(&snapshot);
        assert!(screen.contains("scan wifi +bt waiting for a second node"), "{screen}");
    }

    #[test]
    fn header_shows_scan_stopped_in_yellow_when_bridge_reports_stopped() {
        let snapshot = Snapshot { scan: Some(ScanMode::Stopped), ..busy() };
        assert_eq!(scan_shown(&snapshot), ("scan stopped".to_owned(), Some(Color::Yellow)));
        assert!(rendered(&snapshot).contains("scan stopped"), "{}", rendered(&snapshot));
    }

    #[test]
    fn header_shows_scan_placeholder_when_bridge_has_not_reported() {
        let snapshot = Snapshot { scan: None, pool: None, ..busy() };
        assert_eq!(scan_shown(&snapshot), ("scan —".to_owned(), Some(Color::DarkGray)));
        assert!(rendered(&snapshot).contains("scan —"), "{}", rendered(&snapshot));
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
    fn view_omits_gps_status_when_no_receiver_is_read() {
        // `--no-gps`, or a run with no bridge and no search: there is nothing to report on.
        // The key help names `g gps` on every screen.
        assert!(!rendered(&busy()).replace("g gps", "").contains("gps"));
    }

    #[test]
    fn view_displays_no_receiver_when_neither_host_nor_bridge_has_one() {
        let none = with_gps(GpsStatus::NoReceiver, GpsCounters::default(), PositionSource::Static);
        assert!(rendered(&none).contains("gps no receiver"));
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
    fn view_warns_no_drivable_nodes_when_no_node_holds_a_slot() {
        // Every node heartbeating, none reachable, so there is nothing to partition.
        let mut snapshot = busy();
        snapshot.nodes = vec![unslotted(0x21), unslotted(0x22), unslotted(0x23)];
        snapshot.alive = 3;
        snapshot.assignable = 0;

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut idle())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("3 alive, none usable"), "got {rendered}");
        assert!(!rendered.contains("all nodes silent"), "they are all heartbeating");
    }

    #[test]
    fn view_names_undrivable_nodes_when_only_nodes_heard_run_another_minor() {
        // Heartbeating, so not silent, though none of them is alive to this host.
        let mut snapshot = busy();
        snapshot.nodes = vec![outdated(0x21), outdated(0x22)];
        snapshot.alive = 0;
        snapshot.assignable = 0;
        snapshot.needs_update = 2;

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut idle())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("2 need update"), "got {rendered}");
        assert!(!rendered.contains("all nodes silent"), "they are heartbeating: {rendered}");
    }

    #[test]
    fn view_displays_planner_status_when_rendering_header() {
        let mut empty = busy();
        empty.nodes.clear();
        empty.alive = 0;
        empty.assignable = 0;
        let mut waiting = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        waiting.draw(|frame| draw(frame, &empty, &mut idle())).expect("drawing");
        assert!(waiting.backend().to_string().contains("waiting for nodes"));

        let mut snapshot = busy();
        for node in &mut snapshot.nodes {
            node.share = node.share.or_else(|| assigned(0x84).share);
        }
        snapshot.nodes[1].share = None;
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut idle())).expect("drawing");
        assert!(terminal.backend().to_string().contains("4 of 5"));

        // A lone node holds the whole pool. The header says nothing extra about it.
        let mut lone = busy();
        for node in &mut lone.nodes[1..] {
            node.share = None;
        }
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &lone, &mut idle())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("1 of 5"));
        assert!(!rendered.contains("rotating"));
    }

    /// The color of the header's top-left border corner.
    fn header_border(snapshot: &Snapshot) -> Option<Color> {
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, snapshot, &mut idle())).expect("drawing");
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

    /// `busy()` with the bridge reporting `fan`.
    fn with_fan(fan: Option<FanStats>) -> Snapshot {
        let mut snapshot = busy();
        if let Some(status) = &mut snapshot.bridge_status {
            status.fan = fan;
        }
        snapshot
    }

    #[test]
    fn header_shows_fan_duty_and_speed_when_bridge_reports_a_reading() {
        let duty = FanDuty::new(40).expect("in range");
        let snapshot = with_fan(Some(FanStats { duty, rpm: Some(1830) }));
        assert_eq!(fan(&snapshot).as_deref(), Some("fan 40% 1830 rpm"));
        assert!(rendered(&snapshot).contains("fan 40% 1830 rpm"));
    }

    #[test]
    fn header_shows_fan_duty_alone_when_bridge_has_no_reading_yet() {
        let duty = FanDuty::new(40).expect("in range");
        let snapshot = with_fan(Some(FanStats { duty, rpm: None }));
        assert_eq!(fan(&snapshot).as_deref(), Some("fan 40%"));
    }

    #[test]
    fn header_omits_fan_when_link_is_down() {
        let duty = FanDuty::new(40).expect("in range");
        let mut snapshot = with_fan(Some(FanStats { duty, rpm: Some(1830) }));
        snapshot.link_up = false;
        assert_eq!(fan(&snapshot), None);
        assert!(!rendered(&snapshot).contains("fan "));
    }

    #[test]
    fn header_omits_fan_when_bridge_has_no_carrier() {
        let snapshot = with_fan(None);
        assert_eq!(fan(&snapshot), None);
        assert!(!rendered(&snapshot).contains("fan "));
    }

    /// `busy()` with the bridge reporting `bluetooth`.
    fn with_bluetooth(bluetooth: BluetoothStatus) -> Snapshot {
        let mut snapshot = busy();
        if let Some(status) = &mut snapshot.bridge_status {
            status.bluetooth = bluetooth;
        }
        snapshot
    }

    /// A bonded host on an encrypted link, at `mtu`.
    fn central(mtu: u16) -> Central {
        Central { encrypted: true, bonded: true, mtu, phy: 2, interval_us: 30_000 }
    }

    #[test]
    fn header_names_usb_host_when_run_is_over_usb() {
        assert!(rendered(&busy()).contains("— host usb, link up"));
    }

    #[test]
    fn header_shows_bluetooth_link_when_run_is_over_bluetooth() {
        let snapshot = with_bluetooth(BluetoothStatus {
            owner: HostOwner::Bluetooth,
            central: Some(central(ATT_MTU)),
            ..BluetoothStatus::default()
        });
        let settings = Settings::default().over(Link::Bluetooth);
        let mut terminal = Terminal::new(TestBackend::new(150, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut idle_with(settings))).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("— host bluetooth, MTU 182, link up"), "{screen}");
    }

    #[test]
    fn header_keeps_link_state_and_fan_when_terminal_is_80_columns() {
        // The line is not wrapped, so each host text must leave the rest on screen.
        let duty = FanDuty::new(40).expect("in range");
        let mut snapshot = with_fan(Some(FanStats { duty, rpm: Some(1830) }));
        for (link, owner) in [(Link::Bluetooth, HostOwner::Bluetooth), (Link::Usb, HostOwner::Usb)]
        {
            if let Some(status) = &mut snapshot.bridge_status {
                status.bluetooth = BluetoothStatus {
                    owner,
                    central: Some(central(ATT_MTU)),
                    ..Default::default()
                };
            }
            let settings = Settings::default().over(link);
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test backend");
            terminal
                .draw(|frame| draw(frame, &snapshot, &mut idle_with(settings)))
                .expect("drawing");
            let screen = terminal.backend().to_string();
            assert!(screen.contains(", link up  fan 40% 1830 rpm"), "{screen}");
        }
    }

    #[test]
    fn header_names_bluetooth_host_alone_when_bridge_has_not_reported() {
        let mut snapshot = busy();
        snapshot.bridge_status = None;
        assert_eq!(host(&snapshot, Link::Bluetooth), "host bluetooth");
        assert_eq!(host(&empty(), Link::Bluetooth), "host bluetooth");
        assert!(rendered(&empty()).contains("waiting for bridge — host usb"));
    }

    #[test]
    fn header_shows_waiting_bluetooth_host_when_usb_holds_bridge() {
        let snapshot = with_bluetooth(BluetoothStatus {
            owner: HostOwner::Usb,
            central: Some(central(ATT_MTU)),
            ..BluetoothStatus::default()
        });
        assert_eq!(host(&snapshot, Link::Usb), "host usb; Bluetooth waits");
        assert!(rendered(&snapshot).contains("host usb; Bluetooth waits, link up"));
    }

    #[test]
    fn header_omits_bluetooth_host_when_bridge_would_not_serve_it() {
        // The C6 drops a central with no bond, so it is not waiting for USB.
        let snapshot = with_bluetooth(BluetoothStatus {
            owner: HostOwner::Usb,
            central: Some(Central { bonded: false, ..central(ATT_MTU) }),
            ..BluetoothStatus::default()
        });
        assert_eq!(host(&snapshot, Link::Usb), "host usb");
    }

    #[test]
    fn header_omits_bluetooth_link_when_link_is_down() {
        // The last report describes a host that may have gone.
        let mut snapshot = with_bluetooth(BluetoothStatus {
            owner: HostOwner::Bluetooth,
            central: Some(central(ATT_MTU)),
            ..BluetoothStatus::default()
        });
        snapshot.link_up = false;
        assert_eq!(host(&snapshot, Link::Bluetooth), "host bluetooth");
        assert_eq!(host(&snapshot, Link::Usb), "host usb");
    }

    #[test]
    fn planning_reports_waiting_in_yellow_when_no_node_has_been_seen() {
        let mut snapshot = busy();
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
        for node in &mut snapshot.nodes {
            node.share = None;
        }
        assert!(!snapshot.nodes.is_empty());
        snapshot.alive = 0;
        snapshot.assignable = 0;
        let span = planning(&snapshot);
        assert_eq!(span.content, "all nodes silent");
        assert_eq!(span.style.fg, Some(Color::Red));
    }
}
