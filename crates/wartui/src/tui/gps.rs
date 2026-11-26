//! The GPS modal (`g`) says why a receiver will not lock, or locks badly, which the header's one
//! phrase cannot: the fix on the left, the sky on the right. It is read-only and redraws from each
//! frame's view.
//!
//! The accuracy row is HDOP times a nominal ranging error, the same estimate the export writes. It
//! is not a measurement: that needs `GST`, which common receivers do not send by default.
//!
//! The left column reads the receiver's current report and never the retained fix, so a lost lock
//! shows as one here while the position chain is still using the last fix. Both columns go blank
//! while no receiver is being read, so a lost one leaves no satellites behind.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use wartui_app::view::{
    Augmentation, FixShown, ReceiverPort, ReceiverStatus, ReceiverView, SatelliteShown, SkyDate,
    View,
};
use wartui_core::nmea::Constellation;

use super::centered_rect;
use crate::text::thousands;

/// Shown for a value the receiver has not given.
const NONE: &str = "—";

/// Draw the GPS modal over the view.
pub(super) fn draw_gps_modal(frame: &mut Frame<'_>, view: &View) {
    let area = centered_rect(78, 24, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::bordered().title(" gps ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body, help] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let [left, right] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(0)]).areas(body);
    let receiver = view.receiver.as_ref();
    frame.render_widget(Paragraph::new(fix_lines(receiver)), left);
    let satellites = receiver.and_then(|r| r.sky.as_ref()).map_or(&[][..], |sky| &sky.satellites);
    frame.render_widget(Paragraph::new(sky_lines(satellites, right)), right);
    frame.render_widget(Paragraph::new("esc close"), help);
}

/// The left column's rows below the status, in order.
const ROWS: [&str; 9] =
    ["sats", "time", "lat", "lon", "altitude", "speed", "accuracy", "port", "sentences"];

/// The left column: what the receiver says about its fix, and how it is being read. Every row but
/// the status is blank while no receiver is being read.
fn fix_lines(receiver: Option<&ReceiverView>) -> Vec<Line<'static>> {
    let reading = receiver.and_then(|r| r.sky.as_ref().map(|sky| (r, sky)));
    let values: [Option<String>; ROWS.len()] = reading.map_or_else(Default::default, |(r, sky)| {
        let used = sky.used.map_or_else(|| NONE.to_owned(), |n| n.to_string());
        let time = match (sky.date, sky.time_secs) {
            (Some(date), Some(secs)) => Some(format!("{} {} UTC", date_text(date), clock(secs))),
            (None, Some(secs)) => Some(format!("{} UTC", clock(secs))),
            _ => None,
        };
        [
            Some(format!("{used} used · {} in view", sky.satellites.len())),
            time,
            sky.lat.map(|lat| format!("{lat:>11.6}")),
            sky.lon.map(|lon| format!("{lon:>11.6}")),
            sky.alt_m.map(|alt| format!("{alt:.1} m")),
            sky.speed_kmh.map(|kmh| format!("{kmh:.1} km/h")),
            sky.hdop
                .zip(sky.accuracy_m)
                .map(|(hdop, accuracy)| format!("±{accuracy:.1} m (hdop {hdop})")),
            match &r.port {
                ReceiverPort::Bridge => Some("bridge".to_owned()),
                ReceiverPort::Host { port, baud } => Some(format!("{port} @ {baud}")),
                ReceiverPort::Unsettled => None,
            },
            Some(format!("{} · {} rejected", thousands(r.sentences), thousands(r.rejected))),
        ]
    });
    let row = |label: &str, value: String| Line::from(format!("{label:<10}{value}"));
    let mut lines = vec![row("status", status(receiver))];
    lines.extend(
        ROWS.iter()
            .zip(values)
            .map(|(label, value)| row(label, value.unwrap_or_else(|| NONE.to_owned()))),
    );
    lines
}

/// A date as `2026-10-10`.
fn date_text(date: SkyDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

/// Seconds since midnight as `15:40:12`.
fn clock(secs: u32) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// The status row: what the receiver reports, or `off` with no receiver read.
pub(super) fn status(receiver: Option<&ReceiverView>) -> String {
    let Some(receiver) = receiver else { return "off".to_owned() };
    match &receiver.status {
        ReceiverStatus::Connecting => "connecting".to_owned(),
        ReceiverStatus::Scanning { port, baud } => format!("looking on {port} @{baud}"),
        ReceiverStatus::NoReceiver => "no receiver found".to_owned(),
        ReceiverStatus::Failed { reason } => format!("failed: {reason}"),
        ReceiverStatus::Reading { fix, silent_secs } => {
            let mut text = fix_text(*fix);
            if let Some(secs) = silent_secs {
                text.push_str(&format!(" · silent {secs} s"));
            }
            text
        }
    }
}

/// What the receiver's own sentences say about its fix.
fn fix_text(fix: FixShown) -> String {
    let suffix = |augmented| match augmented {
        Augmentation::None => "",
        Augmentation::Dgps => " (DGPS)",
        Augmentation::RtkFixed => " (RTK fixed)",
        Augmentation::RtkFloat => " (RTK float)",
        Augmentation::DeadReckoning => " (dead reckoning)",
    };
    match fix {
        FixShown::Searching => "searching".to_owned(),
        FixShown::WaitingForSentences => "waiting for sentences".to_owned(),
        FixShown::ThreeD { augmented } => format!("3D fix{}", suffix(augmented)),
        FixShown::TwoD { augmented } => format!("2D fix{}", suffix(augmented)),
        FixShown::Unspecified { augmented } => format!("fix{}", suffix(augmented)),
    }
}

/// The right column: every satellite in view, in the view's order, as many as `area` holds.
fn sky_lines(satellites: &[SatelliteShown], area: Rect) -> Vec<Line<'static>> {
    if satellites.is_empty() {
        return vec![Line::from("none in view")];
    }
    let mut lines = vec![Line::from("sys   PRN  SNR  used")];
    let room = usize::from(area.height).saturating_sub(1);
    let shown = if satellites.len() > room { room.saturating_sub(1) } else { satellites.len() };
    lines.extend(satellites[..shown].iter().map(|s| {
        let snr = s.snr.map_or_else(|| NONE.to_owned(), |snr| snr.to_string());
        let text = format!(
            "{:<4} {:>4} {:>4}  {}",
            system(s.system),
            s.prn,
            snr,
            if s.used { "Y" } else { "N" }
        );
        let style = if s.used { Style::new().fg(Color::Green) } else { Style::new() };
        Line::styled(text, style)
    }));
    if shown < satellites.len() {
        lines.push(Line::from(format!("+{} more", satellites.len() - shown)));
    }
    lines
}

/// A constellation in three letters.
fn system(system: Constellation) -> &'static str {
    match system {
        Constellation::Gps => "GPS",
        Constellation::Glonass => "GLO",
        Constellation::Galileo => "GAL",
        Constellation::BeiDou => "BDS",
        Constellation::Qzss => "QZS",
        Constellation::NavIc => "IRN",
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use std::time::Duration;

    use wartui_core::engine::Snapshot;
    use wartui_core::gps::GpsStatus;
    use wartui_core::nmea::{FixMode, Sky};

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;

    /// `snapshot` drawn at 120×30 with the GPS modal open, as text.
    fn modal(snapshot: &Snapshot) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        let mut ui = idle();
        ui.open_gps();
        terminal.draw(|frame| draw(frame, snapshot, &mut ui)).expect("drawing");
        terminal.backend().to_string()
    }

    #[test]
    fn gps_modal_shows_fix_and_satellites_when_receiver_fixed() {
        let screen = modal(&with_sky(8));
        assert!(screen.contains("3D fix (DGPS)"), "{screen}");
        assert!(screen.contains("48.117300"), "{screen}");
        assert!(screen.contains("km/h"), "{screen}");
        assert!(screen.contains("GPS     1   45  Y"), "{screen}");
        assert!(screen.contains("8 used · 8 in view"), "{screen}");
        assert!(!screen.contains("silent"), "{screen}");
    }

    #[test]
    fn gps_modal_names_bridge_as_port_when_receiver_is_the_bridges() {
        let screen = modal(&with_bridge_sky(8));
        assert!(screen.contains("port      bridge"), "{screen}");
        assert!(!screen.contains("/dev/"), "{screen}");
    }

    #[test]
    fn gps_modal_says_off_when_no_receiver_configured() {
        let screen = modal(&busy());
        assert!(screen.contains("status    off"), "{screen}");
        assert!(screen.contains("none in view"), "{screen}");
    }

    #[test]
    fn gps_modal_shows_more_line_when_satellites_overflow() {
        // 21 rows inside the modal hold the header and 20 satellites, so 40 overflow.
        let screen = modal(&with_sky(40));
        assert!(screen.contains("+21 more"), "{screen}");
    }

    #[test]
    fn gps_modal_says_silent_when_receiver_stops_sending_past_max_age() {
        let mut snapshot = with_sky(8);
        snapshot.now_ms += 12_000;
        let gps = snapshot.gps.as_mut().expect("a receiver");
        gps.max_age = Duration::from_secs(30);
        let screen = modal(&snapshot);
        assert!(!screen.contains("silent"), "{screen}");
        snapshot.gps.as_mut().expect("a receiver").max_age = Duration::from_secs(5);
        let screen = modal(&snapshot);
        assert!(screen.contains("3D fix (DGPS) · silent 12 s"), "{screen}");
    }

    #[test]
    fn gps_modal_shows_date_and_time_when_receiver_reports_both() {
        let screen = modal(&with_sky(8));
        assert!(screen.contains("time      2026-10-10 15:40:12 UTC"), "{screen}");
    }

    #[test]
    fn gps_modal_shows_time_alone_when_receiver_reports_no_date() {
        let mut snapshot = with_sky(8);
        snapshot.gps.as_mut().expect("a receiver").sky.date = None;
        let screen = modal(&snapshot);
        assert!(screen.contains("time      15:40:12 UTC"), "{screen}");
    }

    #[test]
    fn gps_modal_shows_no_satellites_when_receiver_not_read() {
        let mut snapshot = with_sky(8);
        snapshot.gps.as_mut().expect("a receiver").status =
            GpsStatus::Failed("/dev/ttyACM1 is no longer attached".to_owned());
        let screen = modal(&snapshot);
        assert!(screen.contains("none in view"), "{screen}");
        assert!(!screen.contains("GPS     1"), "{screen}");
        assert!(screen.contains("sats      —"), "{screen}");
    }

    #[test]
    fn gps_modal_says_searching_when_gga_reports_no_fix_after_3d_mode() {
        // That quality 0 outranks the mode is `wartui_app::view`'s rule; this is its word.
        let mut snapshot = with_sky(8);
        let gps = snapshot.gps.as_mut().expect("a receiver");
        gps.sky = Sky { quality: Some(0), mode: Some(FixMode::ThreeD), ..Sky::default() };
        assert_eq!(status(view(&snapshot).receiver.as_ref()), "searching");
    }
}
