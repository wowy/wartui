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

pub(super) fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &Snapshot,
    ui: &Ui,
    faults: &[String],
) {
    let c = snapshot.counters;
    let mut lines = Vec::new();

    // What just happened, when it was the operator who made it happen. This
    // takes the first line outright: a key that was refused, and why, matters
    // more in that moment than a running total that will still be there next
    // frame.
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
        // Beside the totals rather than in the fault box: connecting to a
        // bridge that has been buffering beside a fleet produces these as a
        // matter of course, and a line in the fault box would make the
        // ordinary case look like a broken one. It still shows before the
        // first assignment goes out, because that is the case where it is the
        // whole answer to "why has nothing been assigned yet".
        if c.admin_windows_missed > 0 {
            spans.push(Span::raw(format!("  {} held for a live window", c.admin_windows_missed)));
        }
        lines.push(Line::from(spans));
    }

    // A line of its own rather than a span of the totals, so a notice never hides
    // it; plain rather than yellow, since a full ring is expected in a dense area.
    if let Some(drops) = drop_line(&c) {
        lines.push(Line::from(drops));
    }

    // Its own line for the same reason, kept after the upload ends until the next `u`.
    if let Some(status) = ui.upload.status() {
        lines.push(Line::from(format!("  {status}")));
    }

    // Already fitted to the width by `fault_lines`, so nothing here can be
    // clipped and no fault can push another one off the end.
    for fault in faults {
        lines.push(Line::from(Span::styled(fault.clone(), Style::new().fg(Color::Yellow))));
    }

    frame.render_widget(Paragraph::new(lines).dim(), area);
}

/// Sightings the fleet's nodes had no ring room for this session, or `None` while
/// there are none. Most are reported on the next dwell or scan, so this reads as
/// a measure of the rings against the area rather than as a fault.
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

/// Everything that has gone wrong so far, in the order an operator would want
/// to hear it. Empty on a clean run, which is what keeps a clean run looking
/// clean.
pub(super) fn faults(snapshot: &Snapshot) -> Vec<String> {
    let c = snapshot.counters;
    let mut faults = Vec::new();
    // First: with the link down nothing else here is being updated.
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
    // Channels that went into no share. The planner leaving them out is right and
    // completely invisible, since what remains is an ordinary partition of the part
    // the fleet can reach — so the only account of them is here.
    //
    // Two causes, told apart the way `Plan::unreachable` says they can be: every
    // radio tunes 2.4 GHz, so a 2.4 GHz channel is in there only when no node is
    // sniffing at all.
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
        // Every line failing its checksum is a receiver talking at a rate nobody is
        // listening at. Worth saying only when the rate was the operator's: one the
        // ladder chose already produced valid sentences at that rate, so a flood of
        // rejects afterwards is a receiver that changed or a cable that is failing,
        // and `--gps-baud` is not the answer to either.
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
    // The one fault naming a node the operator owns: a fleet half-way through a
    // reflash is invisible to the table above.
    if c.incompatible > 0 {
        faults.push(format!("{} frames from an older firmware — reflash", c.incompatible));
    }
    if c.foreign_fleet > 0 {
        faults.push(format!("{} frames from a vendor fleet", c.foreign_fleet));
    }
    if c.foreign_admin > 0 {
        faults.push(format!("{} admin frames from another core", c.foreign_admin));
    }
    // The bridge's own count runs from its boot and is dominated by what it
    // dropped before anyone was listening; only this capture's loss belongs here.
    if let Some(status) = snapshot.bridge_status
        && status.dropped_since_attach > 0
    {
        faults.push(format!("bridge dropped {}", status.dropped_since_attach));
    }
    // A restart underneath a running capture is a fault even with nothing failing
    // now: the peer table came back empty and the gap is in the data. A power-on
    // is not one — that is how a capture starts.
    if let Some(bridge) = &snapshot.bridge
        && let Some(reason) = restart_fault(bridge)
    {
        faults.push(reason);
    }
    faults
}

/// How to name a bridge restart in the fault box, or `None` for the one that
/// is not a fault.
fn restart_fault(bridge: &BridgeInfo) -> Option<String> {
    // Ahead of the cause, being the more specific statement: it reset itself.
    if matches!(bridge.last_phase, LoopPhase::TxStalled) {
        return Some("bridge rebooted itself: USB transmit had stalled".to_owned());
    }
    match bridge.reset_cause {
        ResetCause::PowerOn => None,
        ResetCause::Software => Some("bridge rebooted (firmware reset)".to_owned()),
        ResetCause::Watchdog => Some("bridge rebooted (watchdog)".to_owned()),
        ResetCause::Lockup => Some("bridge rebooted (CPU lockup)".to_owned()),
        ResetCause::Brownout => Some("bridge rebooted (brownout)".to_owned()),
        ResetCause::External => Some("bridge rebooted (reset over USB)".to_owned()),
        ResetCause::Unknown => Some("bridge rebooted (cause unknown)".to_owned()),
    }
}

/// How many lines of faults the footer may take before it starts summarizing.
///
/// The fleet table has to keep some of the terminal.
const MAX_FAULT_LINES: usize = 3;

/// Fit the faults into lines no wider than the terminal.
///
/// Joining them all with two spaces and letting the terminal clip the overflow
/// is how the most important fault ends up invisible — and the most important
/// fault is usually the longest, because it carries an error string from the
/// operating system.
pub(super) fn fault_lines(faults: &[String], width: u16) -> Vec<String> {
    // A width this small is not a terminal anyone is reading, but the
    // arithmetic below still has to terminate.
    let width = usize::from(width).max(8);
    let (mut lines, ends) = pack(faults, width);
    if lines.len() <= MAX_FAULT_LINES {
        return lines;
    }
    // Something has to give, and it is a whole line rather than the tail of one.
    // Gluing "+2 more" onto half of "Permission denied (os error 13)" leaves a
    // message that looks complete and says something else.
    let kept = MAX_FAULT_LINES - 1;
    // A fault the cut lands in the middle of is not being shown either, so it
    // counts. Otherwise the notice claims nothing is missing while the line
    // above it stops mid-word.
    let hidden = ends.iter().filter(|end| **end >= kept).count();
    lines.truncate(kept);
    lines.push(marker(hidden, width));
    lines
}

/// Lay the faults out, in however many lines that takes.
///
/// Returns the lines, and for each fault the last line it reaches — which is
/// what tells the caller what a cut would cost.
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
    // Narrower than "+1" is not a terminal either, but the notice that something
    // is missing must not itself become the thing that gets clipped.
    count.chars().take(width).collect()
}

/// One fault, split across as many lines as its own length needs.
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
        // Unwrapped, a reason this long would show its first clause on a narrow
        // terminal and nothing else, so `fault_lines` breaks it up instead.
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
        // Three lines of a twenty-column terminal cannot hold this, and the half
        // naming what to do about it is the half that would go.
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
        // The notice takes a line of its own: taking its room out of the last fault
        // would leave a sentence that reads as complete and says something the OS
        // never said.
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
        // Not a terminal anyone uses, but nothing the footer draws may be wider
        // than the frame.
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
        // The tail of the reason is the part that names what to do about it.
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

        // A rate the operator chose is the one that `--gps-baud` can answer for.
        let mistuned = with_pinned_gps(
            GpsStatus::Searching,
            GpsCounters { sentences: 0, fixes: 0, rejected: 300 },
        );
        assert!(rendered(&mistuned).contains("wrong --gps-baud"));
    }

    #[test]
    fn view_reports_unreadable_lines_without_blaming_baud_flag_when_baud_rate_was_auto_detected() {
        // The ladder settles on a rate by getting valid sentences out of it, so
        // unreadable lines afterwards are a receiver that changed or a cable that is
        // failing. Sending the operator to `--gps-baud` would send them nowhere.
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
        // Searching is the default, so most captures with no receiver are captures
        // where there was never going to be one. Saying so on every one of them
        // would put a fault on the view where there is no fault.
        let none = with_gps(GpsStatus::NoReceiver, GpsCounters::default(), PositionSource::Static);
        let drawn = rendered(&none);
        assert!(!drawn.contains("gps"), "{drawn}");
        // A static position still keeps the unpositioned warning off screen.
        assert!(!drawn.contains("pos none"), "{drawn}");
    }

    /// A capture whose only fault is the bridge dropping frames.
    ///
    /// The other counters are cleared deliberately. The bridge's fault is
    /// pushed last and the footer summarises everything past
    /// `MAX_FAULT_LINES`, so a fixture carrying `busy()`'s other seven faults
    /// would be testing the packing rather than the branch.
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

    /// The footer's running-totals line, identified by `frames` rather than
    /// position: the fleet table's `lost` column header, drawn once any node
    /// has lost a batch, would otherwise match a plain substring search for "lost".
    fn totals_line(screen: &str) -> String {
        screen.lines().find(|line| line.contains("frames")).unwrap_or_default().to_owned()
    }

    #[test]
    fn view_shows_lost_total_when_batches_lost_is_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.batches_lost = 7;
        let screen = rendered(&snapshot);
        assert!(totals_line(&screen).contains("lost 7"), "{screen}");

        // `busy()` itself has nothing lost, the same rule `admin` follows.
        let screen = rendered(&busy());
        assert!(!totals_line(&screen).contains("lost"), "{screen}");
    }

    #[test]
    fn view_shows_dup_total_when_duplicate_batches_is_nonzero() {
        let mut snapshot = busy();
        snapshot.counters.duplicate_batches = 3;
        let screen = rendered(&snapshot);
        assert!(totals_line(&screen).contains("dup 3"), "{screen}");

        // `busy()` itself has nothing duplicated, the same rule `lost` follows.
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
        // A kind with nothing dropped is left out rather than shown as 0.
        assert!(!line.contains("ble drop"), "{screen}");
        assert!(!line.contains("frames"), "a line of its own, not a span of the totals");

        snapshot.counters.ble_dropped = 4;
        assert!(rendered(&snapshot).contains("wifi drop 12  ble drop 4"));
    }

    #[test]
    fn footer_keeps_drop_line_when_notice_shown() {
        // The notice takes the totals line; the drops are on their own line so
        // they stay.
        let mut snapshot = busy();
        snapshot.counters.wifi_dropped = 12;
        snapshot.counters.ble_dropped = 4;
        let mut ui = Ui { notice: Some(("a notice".to_owned(), snapshot.now_ms)), ..Ui::default() };
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

        // Every capture starts with a bridge that was powered on. Matched on the
        // full phrase: nodes have their own `rebooted` column.
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
