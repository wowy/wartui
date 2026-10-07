//! The fault box is empty on a clean run. Counts a healthy capture also produces, such
//! as dropped sightings and missed admin windows, sit outside it, so a clean run looks
//! clean.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wartui_bridge::BridgeInfo;
use wartui_core::engine::{Counters, Snapshot};
use wartui_core::gps::GpsStatus;
use wartui_proto::link::{LoopPhase, ResetCause};
use wartui_proto::plan;

use super::ui::Ui;

/// Draw the footer into `area`. `faults` are already wrapped by [`fault_lines`].
pub(super) fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &Snapshot,
    ui: &Ui,
    faults: &[String],
) {
    let c = snapshot.counters;
    let mut lines = Vec::new();

    // A notice replaces the key help and totals for a few seconds. It says what just
    // happened, a refused key or an upload's progress, and the totals will still be
    // there next frame.
    if let Some(notice) = ui.notice(snapshot.now_ms) {
        lines.push(Line::from(Span::styled(
            format!(" {notice}"),
            Style::new().fg(Color::Black).bg(Color::Yellow),
        )));
    } else {
        let mut spans = vec![Span::styled(
            " q quit  ↑↓ select  b bluetooth  c settings  r clear  R clear fleet  u upload ",
            Style::new().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::raw(format!(
            "  frames {}  obs {}  beats {}  stored {}",
            c.frames, c.observations, c.heartbeats, snapshot.store.written
        )));
        if c.admin_sent > 0 {
            spans.push(Span::raw(format!("  admin {}/{}", c.admin_acked, c.admin_sent)));
        }
        if c.batches_lost > 0 {
            spans.push(Span::raw(format!("  lost {}", c.batches_lost)));
        }
        if c.duplicate_batches > 0 {
            spans.push(Span::raw(format!("  dup {}", c.duplicate_batches)));
        }
        // Missed windows sit with the totals, not the faults: connecting to a bridge
        // that has been buffering produces them normally. It shows before the first
        // assignment too, where it explains why nothing has been assigned yet.
        if c.admin_windows_missed > 0 {
            spans.push(Span::raw(format!("  {} held for a live window", c.admin_windows_missed)));
        }
        lines.push(Line::from(spans));
    }

    // Its own line, so a notice never hides it. Plain, not yellow, because a full
    // pending buffer is normal in a dense area.
    if let Some(drops) = drop_line(&c) {
        lines.push(Line::from(drops));
    }

    // Its own line for the same reason. It stays after the upload ends, until the
    // next `u`.
    if let Some(status) = ui.upload().status() {
        lines.push(Line::from(format!("  {status}")));
    }

    // Already fitted by `fault_lines`, so nothing here is clipped.
    for fault in faults {
        lines.push(Line::from(Span::styled(fault.clone(), Style::new().fg(Color::Yellow))));
    }

    frame.render_widget(Paragraph::new(lines).dim(), area);
}

/// Sightings dropped for lack of pending-buffer room this session, or `None`. Most are
/// re-reported on the next dwell, so this measures how dense the area is, not a fault.
pub(super) fn drop_line(c: &Counters) -> Option<String> {
    if c.wifi_dropped == 0 && c.ble_dropped == 0 {
        return None;
    }
    let mut line = String::new();
    if c.wifi_dropped > 0 {
        line.push_str(&format!("  wifi drop {}", c.wifi_dropped));
    }
    if c.ble_dropped > 0 {
        line.push_str(&format!("  ble drop {}", c.ble_dropped));
    }
    Some(line)
}

/// Every fault so far, most urgent first. Empty on a clean run.
pub(super) fn faults(snapshot: &Snapshot) -> Vec<String> {
    let c = snapshot.counters;
    let mut faults = Vec::new();
    // First, because with the link down nothing else here updates.
    if let Some(error) = &snapshot.link_error
        && !snapshot.link_up
    {
        faults.push(format!("link down: {error}"));
    }
    if snapshot.store.dropped > 0 {
        faults.push(format!("store dropped {}", snapshot.store.dropped));
    }
    if c.admin_failed > 0 {
        faults.push(format!("{} assignments unacknowledged", c.admin_failed));
    }
    if c.admin_unadopted > 0 {
        faults.push(format!("{} assignments acknowledged but not adopted", c.admin_unadopted));
    }
    if c.peer_table_full > 0 {
        faults.push("peer table full".to_owned());
    }
    // Channels in no share, which nothing else reports. There are two causes. No
    // sniffing radio can tune them: 5 GHz channels in a fleet of C6s. Or every node is
    // scanning Bluetooth, so nothing sniffs at all. Every radio tunes 2.4 GHz, so a
    // missing 2.4 GHz channel means the second.
    if let Some(plan) = snapshot.plan
        && !plan.unreachable().is_empty()
    {
        let no_sniffer = plan.unreachable().indices().any(|idx| !plan::is_five_ghz(idx));
        faults.push(if no_sniffer {
            format!(
                "every node in this fleet is scanning Bluetooth, so none of the {} channels of \
                 the {} pool is being swept",
                plan.unreachable().len(),
                snapshot.pool
            )
        } else {
            format!(
                "{} channels of the {} pool are 5 GHz and no node in this fleet is sniffing with \
                 a 5 GHz radio, so they are not being scanned",
                plan.unreachable().len(),
                snapshot.pool
            )
        });
    }
    if let Some(gps) = &snapshot.gps {
        if let GpsStatus::Failed(reason) = &gps.status {
            faults.push(format!("gps unreadable: {reason}"));
        }
        // Lines failing their checksum with no fix suggest the wrong baud rate. Blame
        // `--gps-baud` only when the operator set it. An auto-detected rate already
        // decoded, so later rejects mean a changed receiver or a bad cable.
        if gps.counters.fixes == 0 && gps.counters.rejected > 20 {
            let rejected = gps.counters.rejected;
            faults.push(if gps.pinned_baud {
                format!("gps: {rejected} unreadable lines and no fix — wrong --gps-baud?")
            } else {
                match &gps.settled {
                    Some((port, baud)) => {
                        format!("gps: {rejected} unreadable lines and no fix from {port} @{baud}")
                    }
                    None => format!("gps: {rejected} unreadable lines and no fix"),
                }
            });
        }
    }
    if c.undecodable > 0 {
        faults.push(format!("undecodable {}", c.undecodable));
    }
    if c.garbled > 0 {
        faults.push(format!("garbled {}", c.garbled));
    }
    // A fleet half-way through a reflash is invisible to the fleet table, so this
    // is the only place it shows.
    if c.incompatible > 0 {
        faults.push(format!("{} frames from an older firmware — reflash", c.incompatible));
    }
    if c.foreign_fleet > 0 {
        faults.push(format!("{} frames from a vendor fleet", c.foreign_fleet));
    }
    if c.foreign_admin > 0 {
        faults.push(format!("{} admin frames from another core", c.foreign_admin));
    }
    // Only this capture's loss. The bridge's own count runs from its boot, mostly
    // frames dropped before anyone was listening.
    if let Some(status) = snapshot.bridge_status
        && status.dropped_since_attach > 0
    {
        faults.push(format!("bridge dropped {}", status.dropped_since_attach));
    }
    // A restart under a running capture is a fault even when nothing fails now: the
    // peer table came back empty and the data has a gap. A power-on is how a capture
    // starts, so it is not one.
    if let Some(bridge) = &snapshot.bridge
        && let Some(reason) = restart_fault(bridge)
    {
        faults.push(reason);
    }
    faults
}

/// The fault-box line for a bridge restart, or `None` for a power-on.
fn restart_fault(bridge: &BridgeInfo) -> Option<String> {
    // Ahead of the cause, because it is more specific: the bridge reset itself.
    if matches!(bridge.last_phase, LoopPhase::TxStalled) {
        return Some("bridge rebooted itself: USB transmit had stalled".to_owned());
    }
    let reason = match bridge.reset_cause {
        ResetCause::PowerOn => None,
        ResetCause::Software => Some("bridge rebooted (firmware reset)"),
        ResetCause::Watchdog => Some("bridge rebooted (watchdog)"),
        ResetCause::Lockup => Some("bridge rebooted (CPU lockup)"),
        ResetCause::Brownout => Some("bridge rebooted (brownout)"),
        ResetCause::External => Some("bridge rebooted (reset over USB)"),
        ResetCause::Unknown => Some("bridge rebooted (cause unknown)"),
    };
    reason.map(str::to_owned)
}

/// The most lines the fault box takes before it summarises, so the fleet table keeps
/// its room.
const MAX_FAULT_LINES: usize = 3;

/// Wrap faults to the terminal width rather than let it clip them.
///
/// The most important fault is usually the longest, because it carries an OS error.
/// Clipped, it would be the one that disappears.
pub(super) fn fault_lines(faults: &[String], width: u16) -> Vec<String> {
    // Nobody reads a terminal this narrow, but the arithmetic must still terminate.
    let width = usize::from(width).max(8);
    let (mut lines, ends) = pack(faults, width);
    if lines.len() <= MAX_FAULT_LINES {
        return lines;
    }
    // Drop whole lines, never part of one. "+2 more" glued onto half of "Permission
    // denied (os error 13)" looks complete and says something else.
    let kept = MAX_FAULT_LINES - 1;
    // A fault the cut splits counts as hidden.
    let hidden = ends.iter().filter(|end| **end >= kept).count();
    lines.truncate(kept);
    lines.push(marker(hidden, width));
    lines
}

/// Lay the faults out in as many lines as they take.
///
/// Returns the lines, and for each fault the last line it reaches. That tells the
/// caller which faults a cut would hide.
fn pack(faults: &[String], width: usize) -> (Vec<String>, Vec<usize>) {
    let mut lines: Vec<String> = Vec::new();
    let mut ends = Vec::with_capacity(faults.len());
    for fault in faults {
        let room = |line: &String| line.chars().count() + 2 + fault.chars().count() <= width;
        if lines.last().is_some_and(room) {
            if let Some(line) = lines.last_mut() {
                line.push_str("  ");
                line.push_str(fault);
            }
        } else {
            lines.extend(chunks(fault, width));
        }
        ends.push(lines.len().saturating_sub(1));
    }
    (lines, ends)
}

/// How much is not on screen, in the widest wording that fits.
fn marker(hidden: usize, width: usize) -> String {
    let count = format!("+{hidden}");
    for wording in [format!("  +{hidden} more"), format!("+{hidden} more"), count.clone()] {
        if wording.chars().count() <= width {
            return wording;
        }
    }
    // Narrower than "+1" is not a real terminal either, but the marker itself must
    // not be clipped.
    count.chars().take(width).collect()
}

/// One fault, split across as many lines as its length needs.
fn chunks(fault: &str, width: usize) -> Vec<String> {
    let room = width.saturating_sub(2).max(1);
    let mut out = Vec::new();
    let mut rest: Vec<char> = fault.chars().collect();
    while !rest.is_empty() {
        let take = rest.len().min(room);
        let line: String = rest.drain(..take).collect();
        out.push(format!("  {line}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use wartui_core::gps::GpsCounters;
    use wartui_core::position::PositionSource;
    use wartui_proto::plan::{ChannelPool, Job, Radio, plan_for};

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;

    #[test]
    fn view_displays_faults_only_when_they_have_occurred() {
        let mut clean = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        clean.draw(|frame| draw(frame, &empty(), &mut Ui::default())).expect("drawing");
        assert!(!clean.backend().to_string().contains("dropped"));

        let mut faulty = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        faulty.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        let rendered = faulty.backend().to_string();
        assert!(rendered.contains("store dropped 7"));
        assert!(rendered.contains("admin frames from another core"));
        assert!(rendered.contains("older firmware"));
        assert!(rendered.contains("vendor fleet"));
    }

    const BUSY: &str = "could not open /dev/cu.usbmodem101: Device or resource busy";

    #[test]
    fn fault_lines_wraps_long_message_when_fault_exceeds_terminal_width() {
        // Unwrapped, a narrow terminal would show only the first clause.
        let fault = format!("link down: {BUSY}");
        let lines = fault_lines(std::slice::from_ref(&fault), 40);
        assert!(lines.len() > 1, "it does not fit on one line: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let rejoined: String = lines.iter().map(|l| l.trim_start()).collect();
        assert_eq!(rejoined, fault, "every character of it is on screen somewhere");
    }

    #[test]
    fn fault_lines_combines_messages_into_one_line_when_faults_fit_within_width() {
        let faults = ["store dropped 7".to_owned(), "unparsed 3".to_owned()];
        assert_eq!(fault_lines(&faults, 200), vec!["  store dropped 7  unparsed 3".to_owned()]);
    }

    #[test]
    fn fault_lines_appends_overflow_count_when_faults_exceed_footer_capacity() {
        let faults: Vec<String> = (0..12).map(|n| format!("fault number {n} of twelve")).collect();
        let lines = fault_lines(&faults, 40);
        assert_eq!(lines.len(), MAX_FAULT_LINES, "the fleet table keeps the rest of the screen");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:?}");
        let last = lines.last().expect("a line");
        assert!(last.contains("more"), "how many are not shown: {last}");
    }

    #[test]
    fn fault_lines_appends_truncated_indicator_when_single_fault_exceeds_box_capacity() {
        // Three lines of twenty columns cannot hold this, and the half that names
        // the fix is the half that goes.
        let fault = format!("link down: {BUSY}");
        let lines = fault_lines(std::slice::from_ref(&fault), 20);
        assert_eq!(lines.len(), MAX_FAULT_LINES, "{lines:?}");
        let last = lines.last().expect("a line");
        assert_eq!(last, "  +1 more", "the operator is told the reason is cut short");
        let shown: String = lines[..MAX_FAULT_LINES - 1].iter().map(|l| l.trim_start()).collect();
        assert!(fault.starts_with(&shown), "what is shown is really the start of it: {shown}");
    }

    #[test]
    fn fault_lines_preserves_preceding_messages_when_overflow_notice_is_added() {
        // The marker takes its own line. Taking its room from the last fault would
        // leave a sentence that looks complete and says something the OS never said.
        let faults = [format!("link down: {BUSY}"), "store dropped 4".to_owned()];
        let lines = fault_lines(&faults, 30);
        let last = lines.last().expect("a line");
        assert_eq!(last, "  +2 more", "both faults are cut, and it says so: {lines:?}");
        assert!(
            lines[..MAX_FAULT_LINES - 1].iter().all(|l| !l.contains('+')),
            "no fault shares a line with the notice: {lines:?}"
        );
        let shown: String = lines[..MAX_FAULT_LINES - 1].iter().map(|l| l.trim_start()).collect();
        assert!(faults[0].starts_with(&shown), "{shown}");
    }

    #[test]
    fn fault_lines_keeps_overflow_notice_within_bounds_when_terminal_width_is_narrow() {
        // Nobody uses a terminal this narrow, but nothing the footer draws may be
        // wider than the frame.
        let faults: Vec<String> = (0..2000).map(|n| format!("fault {n}")).collect();
        for width in [1_u16, 8, 12] {
            let lines = fault_lines(&faults, width);
            let room = usize::from(width).max(8);
            assert!(lines.iter().all(|l| l.chars().count() <= room), "at {width}: {lines:?}");
            assert!(lines.last().expect("a line").contains('+'), "at {width}: {lines:?}");
        }
    }

    #[test]
    fn view_displays_link_down_reason_when_terminal_is_narrow() {
        let mut snapshot = empty();
        snapshot.link_error = Some(BUSY.to_owned());
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("link down"), "{screen}");
        // The tail of the reason names the fix.
        assert!(screen.contains("busy"), "{screen}");
    }

    #[test]
    fn view_displays_link_error_details_when_link_is_down() {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("test backend");
        terminal.draw(|frame| draw(frame, &empty(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("waiting for bridge"));
        assert!(rendered.contains("no bridge found"));
    }

    #[test]
    fn view_reports_gps_fault_when_receiver_is_unreadable_or_baud_rate_is_wrong() {
        let failed = with_gps(
            GpsStatus::Failed("/dev/cu.gps: No such file or directory".to_owned()),
            GpsCounters::default(),
            PositionSource::Static,
        );
        assert!(rendered(&failed).contains("gps unreadable"));

        // `--gps-baud` is the answer only when the operator chose the rate.
        let mistuned = with_pinned_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
        );
        assert!(rendered(&mistuned).contains("wrong --gps-baud"));
    }

    #[test]
    fn view_reports_unreadable_lines_without_blaming_baud_flag_when_baud_rate_was_auto_detected() {
        // The search settles on a rate only after valid sentences at it, so later
        // unreadable lines mean a changed receiver or a failing cable. `--gps-baud`
        // would not help.
        let noisy = with_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
            PositionSource::Static,
        );
        let drawn = rendered(&noisy);
        assert!(!drawn.contains("--gps-baud"), "{drawn}");
        assert!(drawn.contains("300 unreadable lines"), "{drawn}");
    }

    #[test]
    fn view_suppresses_gps_fault_line_when_no_receiver_was_found() {
        // The search runs by default, and most captures have no receiver. Saying so
        // every time would show a fault where there is none.
        let none = with_gps(GpsStatus::NoReceiver, GpsCounters::default(), PositionSource::Static);
        let drawn = rendered(&none);
        assert!(!drawn.contains("gps"), "{drawn}");
        // A static position still keeps the unpositioned warning off screen.
        assert!(!drawn.contains("pos none"), "{drawn}");
    }

    /// A capture whose only fault is the bridge dropping frames.
    ///
    /// The other counters are cleared. The bridge's fault is pushed last, and the footer
    /// summarises past `MAX_FAULT_LINES`, so `busy()`'s other seven faults would test
    /// the packing rather than this branch.
    fn with_bridge_drops(dropped_since_attach: u32) -> Snapshot {
        let mut snapshot = busy();
        snapshot.counters = Counters::default();
        snapshot.store.dropped = 0;
        snapshot.bridge_status.as_mut().expect("busy() has a bridge").dropped_since_attach =
            dropped_since_attach;
        snapshot
    }

    #[test]
    fn view_reports_bridge_drops_only_when_drops_occur_during_current_capture() {
        let screen = rendered(&with_bridge_drops(5));
        assert!(screen.contains("bridge dropped 5"), "{screen}");

        // `busy()`'s bridge dropped 1300 frames before this host attached.
        let screen = rendered(&busy());
        assert!(!screen.contains("bridge dropped"), "{screen}");
    }

    /// The footer's totals line, found by `frames`. A plain search for "lost" would
    /// also match the fleet table's `lost` column header.
    fn totals_line(screen: &str) -> String {
        screen.lines().find(|line| line.contains("frames")).unwrap_or_default().to_owned()
    }

    #[test]
    fn view_shows_lost_total_when_batches_lost_is_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.batches_lost = 7;
        let screen = rendered(&snapshot);
        assert!(totals_line(&screen).contains("lost 7"), "{screen}");

        // `busy()` has nothing lost, so the total is left out, as `admin` is.
        let screen = rendered(&busy());
        assert!(!totals_line(&screen).contains("lost"), "{screen}");
    }

    #[test]
    fn view_shows_dup_total_when_duplicate_batches_is_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.duplicate_batches = 3;
        let screen = rendered(&snapshot);
        assert!(totals_line(&screen).contains("dup 3"), "{screen}");

        // `busy()` has nothing duplicated, so the total is left out, as `lost` is.
        let screen = rendered(&busy());
        assert!(!totals_line(&screen).contains("dup"), "{screen}");
    }

    #[test]
    fn footer_hides_drop_line_when_both_counts_zero() {
        let screen = rendered(&busy());
        assert!(!screen.contains("wifi drop"), "{screen}");
        assert!(!screen.contains("ble drop"), "{screen}");
    }

    #[test]
    fn footer_displays_drop_line_when_wifi_count_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.wifi_dropped = 12;
        let screen = rendered(&snapshot);
        let line = screen.lines().find(|l| l.contains("wifi drop")).expect("a drop line");
        assert!(line.contains("wifi drop 12"), "{screen}");
        // A kind with nothing dropped is left out, not shown as 0.
        assert!(!line.contains("ble drop"), "{screen}");
        assert!(!line.contains("frames"), "a line of its own, not a span of the totals");

        snapshot.counters.ble_dropped = 4;
        assert!(rendered(&snapshot).contains("wifi drop 12  ble drop 4"));
    }

    #[test]
    fn footer_keeps_drop_line_when_notice_shown() {
        // The notice takes the totals line. The drops have their own line, so they
        // stay.
        let mut snapshot = busy();
        snapshot.counters.wifi_dropped = 12;
        snapshot.counters.ble_dropped = 4;
        let mut ui = Ui::default();
        ui.say("a notice".to_owned(), &snapshot);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut ui)).expect("drawing");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("a notice"), "{screen}");
        assert!(!screen.contains("stored 800"), "the notice replaced the totals: {screen}");
        assert!(screen.contains("wifi drop 12  ble drop 4"), "{screen}");
    }

    /// A capture whose only fault is that the bridge restarted underneath it.
    fn with_restart(reset_cause: ResetCause, last_phase: LoopPhase) -> Snapshot {
        let mut snapshot = busy();
        snapshot.counters = Counters::default();
        snapshot.store.dropped = 0;
        snapshot.bridge_status.as_mut().expect("busy() has a bridge").dropped_since_attach = 0;
        let bridge = snapshot.bridge.as_mut().expect("busy() has a bridge");
        bridge.reset_cause = reset_cause;
        bridge.last_phase = last_phase;
        snapshot
    }

    #[test]
    fn view_reports_bridge_reboot_when_reset_cause_is_watchdog() {
        let screen = rendered(&with_restart(ResetCause::Watchdog, LoopPhase::Transmit));
        assert!(screen.contains("watchdog"), "{screen}");

        // Every capture starts with a powered-on bridge. Matched on the full phrase,
        // because nodes have their own `rebooted` state.
        let screen = rendered(&with_restart(ResetCause::PowerOn, LoopPhase::Unknown));
        assert!(!screen.contains("bridge rebooted"), "{screen}");
    }

    #[test]
    fn view_reports_usb_tx_stall_when_bridge_reboots_due_to_tx_stalled() {
        let screen = rendered(&with_restart(ResetCause::Software, LoopPhase::TxStalled));
        assert!(screen.contains("USB transmit had stalled"), "{screen}");
    }

    #[test]
    fn view_warns_missing_five_ghz_coverage_when_fleet_has_no_five_ghz_radios() {
        let mut snapshot = busy();
        snapshot.plan = plan_for(ChannelPool::Us, &[Job::Wifi(Radio::TwoPointFour); 2]);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("no node in this fleet is sniffing with a 5 GHz radio"),
            "got {rendered}"
        );
    }

    #[test]
    fn view_warns_no_wifi_sniffing_when_all_nodes_scan_bluetooth() {
        let mut snapshot = busy();
        snapshot.plan = plan_for(ChannelPool::Us, &[Job::Bluetooth]);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("every node in this fleet is scanning Bluetooth"),
            "got {rendered}"
        );
    }
}
