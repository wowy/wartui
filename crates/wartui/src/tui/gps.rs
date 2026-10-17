//! The GPS modal (`g`) says why a receiver will not lock, or locks badly, which the header's one
//! phrase cannot: the fix on the left, the sky on the right. It is read-only and redraws from each
//! snapshot.
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
use wartui_core::engine::Snapshot;
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_core::nmea::{Constellation, FixMode, Satellite, Sky, accuracy_from_hdop};

use super::centered_rect;
use crate::text::thousands;

/// Shown for a value the receiver has not given.
const NONE: &str = "—";

/// Draw the GPS modal over the view.
pub(super) fn draw_gps_modal(frame: &mut Frame<'_>, snapshot: &Snapshot) {
    let area = centered_rect(78, 24, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::bordered().title(" gps ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [body, help] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    let [left, right] =
        Layout::horizontal([Constraint::Length(36), Constraint::Min(0)]).areas(body);
    let (status, gps) = status(snapshot);
    frame.render_widget(Paragraph::new(fix_lines(status, gps)), left);
    let satellites = gps.map_or(&[][..], |gps| &gps.sky.satellites);
    frame.render_widget(Paragraph::new(sky_lines(satellites, right)), right);
    frame.render_widget(Paragraph::new("esc close"), help);
}

/// The left column's rows below the status, in order.
const ROWS: [&str; 9] =
    ["sats", "time", "lat", "lon", "altitude", "speed", "accuracy", "port", "sentences"];

/// The left column: what the receiver says about its fix, and how it is being read. `gps` is the
/// receiver when it is being read; otherwise every row but the status is blank.
fn fix_lines(status: String, gps: Option<&GpsView>) -> Vec<Line<'static>> {
    let values: [Option<String>; ROWS.len()] = gps.map_or_else(Default::default, |gps| {
        let sky = &gps.sky;
        let used = sky.used_count.map_or_else(|| NONE.to_owned(), |n| n.to_string());
        let time = match (sky.date, sky.time) {
            (Some(date), Some(time)) => Some(format!("{date} {} UTC", time.format("%H:%M:%S"))),
            (None, Some(time)) => Some(format!("{} UTC", time.format("%H:%M:%S"))),
            _ => None,
        };
        let c = gps.counters;
        [
            Some(format!("{used} used · {} in view", sky.satellites.len())),
            time,
            sky.lat.map(|lat| format!("{lat:>11.6}")),
            sky.lon.map(|lon| format!("{lon:>11.6}")),
            sky.alt.map(|alt| format!("{alt:.1} m")),
            sky.speed_kmh.map(|kmh| format!("{kmh:.1} km/h")),
            sky.hdop.map(|hdop| format!("±{:.1} m (hdop {hdop})", accuracy_from_hdop(hdop))),
            if gps.via_bridge {
                Some("bridge (Qwiic)".to_owned())
            } else {
                gps.settled.as_ref().map(|(port, baud)| format!("{port} @ {baud}"))
            },
            Some(format!("{} · {} rejected", thousands(c.sentences), thousands(c.rejected))),
        ]
    });
    let row = |label: &str, value: String| Line::from(format!("{label:<10}{value}"));
    let mut lines = vec![row("status", status)];
    lines.extend(
        ROWS.iter()
            .zip(values)
            .map(|(label, value)| row(label, value.unwrap_or_else(|| NONE.to_owned()))),
    );
    lines
}

/// The status row, and the receiver when it is being read so the other rows have something to
/// show.
fn status(snapshot: &Snapshot) -> (String, Option<&GpsView>) {
    let Some(gps) = &snapshot.gps else { return ("off".to_owned(), None) };
    let text = match &gps.status {
        GpsStatus::Connecting => "connecting".to_owned(),
        GpsStatus::Scanning { port, baud } => format!("looking on {port} @{baud}"),
        GpsStatus::NoReceiver => "no receiver found".to_owned(),
        GpsStatus::Failed(reason) => format!("failed: {reason}"),
        GpsStatus::Searching | GpsStatus::Fixed { .. } => {
            let mut text = sky_status(&gps.sky, gps.heard_ms.is_some());
            if let Some(heard) = gps.heard_ms
                && snapshot.now_ms - heard
                    > i64::try_from(gps.max_age.as_millis()).unwrap_or(i64::MAX)
            {
                text.push_str(&format!(" · silent {} s", (snapshot.now_ms - heard) / 1000));
            }
            text
        }
    };
    let reading = matches!(gps.status, GpsStatus::Searching | GpsStatus::Fixed { .. });
    (text, reading.then_some(gps))
}

/// What the receiver's own sentences say about its fix.
fn sky_status(sky: &Sky, heard: bool) -> String {
    let suffix = match sky.quality {
        Some(2) => " (DGPS)",
        Some(4) => " (RTK fixed)",
        Some(5) => " (RTK float)",
        Some(6) => " (dead reckoning)",
        _ => "",
    };
    // `GGA` quality 0 outranks a `GSA` mode: the two arrive separately, and a lost lock reaches
    // `GGA` while the last `GSA` may still say 3D.
    match (sky.mode, sky.quality) {
        (Some(FixMode::NoFix), _) | (_, Some(0)) => "searching".to_owned(),
        (Some(FixMode::ThreeD), _) => format!("3D fix{suffix}"),
        (Some(FixMode::TwoD), _) => format!("2D fix{suffix}"),
        // A receiver that sends no `GSA` still says it has a fix in `GGA`.
        (None, Some(_)) => format!("fix{suffix}"),
        (None, None) if heard => "searching".to_owned(),
        (None, None) => "waiting for sentences".to_owned(),
    }
}

/// The right column: every satellite in view, used first, then strongest first.
fn sky_lines(satellites: &[Satellite], area: Rect) -> Vec<Line<'static>> {
    if satellites.is_empty() {
        return vec![Line::from("none in view")];
    }
    let mut sorted = satellites.to_vec();
    // System and PRN last, so the list holds still between frames.
    sorted.sort_by_key(|s| (!s.used, std::cmp::Reverse(s.snr), s.system, s.prn));
    let mut lines = vec![Line::from("sys   PRN  SNR  used")];
    let room = usize::from(area.height).saturating_sub(1);
    let shown = if sorted.len() > room { room.saturating_sub(1) } else { sorted.len() };
    lines.extend(sorted[..shown].iter().map(|s| {
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
    if shown < sorted.len() {
        lines.push(Line::from(format!("+{} more", sorted.len() - shown)));
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

    use super::*;
    use crate::tui::draw;
    use crate::tui::fixtures::*;
    use crate::tui::ui::Ui;

    /// `snapshot` drawn at 120×30 with the GPS modal open, as text.
    fn modal(snapshot: &Snapshot) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        let mut ui = Ui::default();
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
        assert!(screen.contains("port      bridge (Qwiic)"), "{screen}");
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
        let sky = Sky { quality: Some(0), mode: Some(FixMode::ThreeD), ..Sky::default() };
        assert_eq!(sky_status(&sky, true), "searching");
    }
}
