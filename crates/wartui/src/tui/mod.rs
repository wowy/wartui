//! The fleet view.
//!
//! See the operator's manual (`crates/wartui/README.md`) for a usage guide.
//!
//! Each frame is drawn from the latest [`Snapshot`], published four times a second.
//! Drawing per observation would back-pressure the link, since a busy fleet sends tens
//! of rows a second.

mod blocks;
mod chart;
mod counts;
mod footer;
mod format;
mod gps;
mod header;
mod input;
mod settings;
mod ui;
mod upload;

#[cfg(test)]
mod fixtures;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_core::engine::{Command, Snapshot};

use blocks::draw_blocks;
use chart::draw_chart;
use footer::{buffer_full_line, draw_footer, fault_lines, faults};
use gps::draw_gps_modal;
use header::draw_header;
use input::{Input, is_ctrl_c, quits, spawn_input};
pub use settings::Settings;
use settings::draw_settings_modal;
use ui::Ui;
pub use upload::UploadTarget;
use upload::draw_confirm_modal;

/// Run the view until the operator quits or the engine stops.
///
/// Takes ownership of `stop`, so every way out shuts the capture down the same way and
/// commits the store's last batch. The ways out are a keypress, ctrl-c, or the engine
/// ending on its own.
pub async fn run(
    mut snapshot: watch::Receiver<Arc<Snapshot>>,
    commands: mpsc::Sender<Command>,
    stop: oneshot::Sender<()>,
    settings: Settings,
) -> Result<()> {
    let mut terminal = ratatui::try_init().context("preparing the terminal")?;
    // A paste arrives as one event rather than as keystrokes, so a pasted newline
    // cannot press `Enter`. A terminal that refuses it still types the paste in.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    // ratatui's panic hook restores raw mode and the alternate screen, but not this.
    // Left on, the operator's shell would wrap every paste in escape codes.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        previous(info);
    }));
    let result = view(&mut terminal, &mut snapshot, &commands, settings).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    // Stop the engine only once the terminal is restored, so anything it logs on the
    // way out lands on a readable screen.
    let _ = stop.send(());
    result
}

async fn view(
    terminal: &mut DefaultTerminal,
    snapshot: &mut watch::Receiver<Arc<Snapshot>>,
    commands: &mpsc::Sender<Command>,
    settings: Settings,
) -> Result<()> {
    // Inside `view`, so a failure still restores the terminal and stops the engine.
    let (mut inputs, running) = spawn_input()?;
    let mut ui = Ui::new(settings);
    // Built before the loop, not inside the arm below. See `crate::Terminate`.
    let mut terminate = crate::Terminate::new();
    let outcome = loop {
        let current = snapshot.borrow_and_update().clone();
        ui.clamp(current.nodes.len());
        ui.poll_upload(&current);
        ui.poll_power(&current);
        ui.poll_bridge();
        if let Err(e) = terminal.draw(|frame| draw(frame, &current, &mut ui)) {
            break Err(e).context("drawing the fleet view");
        }

        tokio::select! {
            input = inputs.recv() => match input {
                // ctrl-c always quits, modal or not.
                Some(Input::Key(key)) if is_ctrl_c(key) => break Ok(()),
                // The upload's confirm opens over everything, settings included, so
                // it answers first.
                Some(Input::Key(key)) if ui.upload().confirming() => ui.on_confirm_key(key, &current),
                // Opened only from the fleet view, so nothing else is open under it.
                Some(Input::Key(key)) if ui.power_confirming() => {
                    ui.on_power_key(key, &current, commands);
                }
                // Ahead of `quits()`, so `q` closes the modal rather than the view. Also ahead of
                // the waiting-for-GPS pop-up it may be opened over, which keys reach again once
                // it closes.
                Some(Input::Key(key)) if ui.gps_open() => ui.on_gps_key(key),
                // An open modal gets the key ahead of `quits()`, so `esc` and `q`
                // close the modal, not the view.
                Some(Input::Key(key)) if ui.modal().is_some() => {
                    ui.on_modal_key(key, &current, commands);
                }
                // The waiting-for-GPS pop-up has nothing to back out to, so `q` and `esc` fall
                // through to `quits()` and quit, as in the fleet view.
                Some(Input::Key(key)) if current.awaiting_fix && !quits(key) => {
                    ui.on_fix_wait_key(key, &current, commands);
                }
                Some(Input::Key(key)) if quits(key) => break Ok(()),
                Some(Input::Key(key)) => ui.on_key(key, &current, commands),
                Some(Input::Paste(text)) => ui.on_modal_paste(&text),
                // The input thread died. Carrying on would leave a view nobody can
                // quit.
                None => break Ok(()),
            },
            changed = snapshot.changed() => {
                if changed.is_err() {
                    break Ok(());   // The engine stopped.
                }
            }
            // The same exit as `q`. The terminal is restored, the engine stops, the
            // last batch is committed and the port is released. See
            // `crate::Terminate`.
            () = terminate.recv() => break Ok(()),
        }
    };
    running.store(false, Ordering::Relaxed);
    outcome
}

fn draw(frame: &mut Frame<'_>, snapshot: &Snapshot, ui: &mut Ui) {
    // Faults get their own lines, and only when there are any, so the counters
    // cannot push them off screen.
    let faults = fault_lines(&faults(snapshot), frame.area().width);
    let buffer_full = u16::from(buffer_full_line(&snapshot.counters).is_some());
    let upload = u16::from(ui.upload().status().is_some());
    let footer_height = 1 + buffer_full + upload + u16::try_from(faults.len()).unwrap_or(u16::MAX);
    let area = frame.area();
    let body = Rect { height: area.height.saturating_sub(HEADER_H + footer_height), ..area };
    let wide = body.width >= WIDE_AT;
    let counts = counts::Counts::new(snapshot);
    let strip_min = strip_min(&counts, body.width, wide);
    let grid_height = blocks::wanted_height(snapshot, body.width, wide)
        .min(body.height.saturating_sub(strip_min));
    let [header, strip, grid, footer] = Layout::vertical([
        Constraint::Length(HEADER_H),
        Constraint::Min(strip_min),
        Constraint::Length(grid_height),
        Constraint::Length(footer_height),
    ])
    .areas(area);

    draw_header(frame, header, snapshot);
    draw_strip(frame, strip, snapshot, &counts, wide);
    let (selected, scroll, cols) = ui.grid_mut();
    draw_blocks(frame, grid, snapshot, selected, scroll, cols, wide);
    draw_footer(frame, footer, snapshot, ui, &faults);

    if let Some(lines) = ui.fix_wait_lines(snapshot) {
        draw_confirm_modal(frame, " waiting for GPS ", &lines);
    }
    if let Some(modal) = ui.modal() {
        draw_settings_modal(frame, modal);
    }
    if ui.gps_open() {
        draw_gps_modal(frame, snapshot);
    }
    if let Some(lines) = ui.power_confirm_lines(snapshot) {
        draw_confirm_modal(frame, " power-cycle ", &lines);
    }
    if let Some(lines) = ui.upload().confirm_lines() {
        draw_confirm_modal(frame, " upload to WDGWars ", &lines);
    }
}

/// The header's rows.
const HEADER_H: u16 = 4;

/// The body width from which blocks carry labeled lines and the counts stack beside the chart.
const WIDE_AT: u16 = 100;

/// The strip's least height: its borders, then two stacked count groups in the wide layout, or
/// in the narrow one a counts band over three bar rows and a label row. A narrow band that
/// stacks the counts grows the strip by their height.
fn strip_min(counts: &counts::Counts, width: u16, wide: bool) -> u16 {
    if wide { 10 } else { 2 + counts.band_height(width.saturating_sub(2)).max(4) + 4 }
}

/// The strip under the header: the big unique counts, and the chart of Wi-Fi sightings per
/// channel, under a title of every sighting this capture holds.
fn draw_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &Snapshot,
    counts: &counts::Counts,
    wide: bool,
) {
    let title = format!(" {} sightings ", crate::text::thousands(snapshot.counters.observations));
    let right = " wifi sightings per channel ";
    let title_w = title.chars().count();
    let mut block = Block::bordered().title(title);
    if usize::from(area.width) >= title_w + right.chars().count() + 4 {
        block = block.title_top(Line::from(right).right_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if wide {
        let [left, chart] =
            Layout::horizontal([Constraint::Length(counts.wide_width()), Constraint::Min(0)])
                .areas(inner);
        counts.draw(frame, left, true);
        draw_chart(frame, chart, &snapshot.wifi_by_channel);
    } else {
        let band = counts.band_height(inner.width);
        let [top, chart] =
            Layout::vertical([Constraint::Length(band), Constraint::Min(0)]).areas(inner);
        counts.draw(frame, top, false);
        draw_chart(frame, chart, &snapshot.wifi_by_channel);
    }
}

/// A `width` by `height` box centered in `area`, clipped to it when too big.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::fixtures::*;

    /// Rendering must not panic at any size the terminal might be.
    ///
    /// A layout that divides by a zero-width area or indexes past a short one panics.
    /// That ends the capture inside the alternate screen, where the backtrace is
    /// unreadable.
    #[test]
    fn view_renders_without_panicking_when_given_various_terminal_sizes() {
        let mut gps = Ui::default();
        gps.open_gps();
        let mut lossy = fleet_of(14);
        lossy.nodes[3].state.batches_lost = 2;
        for snapshot in [busy(), empty(), with_sky(40), fleet_of(14), lossy, awaiting_fix()] {
            for (width, height) in [
                (200, 50),
                (138, 38),
                (120, 30),
                (100, 20),
                (99, 20),
                (80, 24),
                (58, 26),
                (40, 10),
                (20, 5),
                (6, 3),
                (1, 1),
            ] {
                for ui in [&mut Ui::default(), &mut gps] {
                    let mut terminal =
                        Terminal::new(TestBackend::new(width, height)).expect("test backend");
                    terminal.draw(|frame| draw(frame, &snapshot, ui)).expect("drawing");
                }
            }
        }
    }

    #[test]
    fn view_draws_fix_wait_pop_up_over_live_view_when_awaiting_fix() {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &awaiting_fix(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains(" waiting for GPS "), "{rendered}");
        assert!(rendered.contains("Waiting for a GPS fix before the scan starts."), "{rendered}");
        assert!(rendered.contains("gps: searching"), "{rendered}");
        assert!(rendered.contains("[ ] skip this wait at every launch"), "{rendered}");
        assert!(
            rendered.contains("space tick · enter scan without a fix · g gps · q quit"),
            "{rendered}"
        );
        assert!(rendered.contains("unique APs"), "the live view behind it: {rendered}");

        terminal.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        assert!(!terminal.backend().to_string().contains("waiting for GPS"), "gone with the wait");
    }

    #[test]
    fn view_draws_gps_modal_over_fix_wait_pop_up_when_both_are_open() {
        let mut ui = Ui::default();
        ui.open_gps();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("test backend");
        terminal.draw(|frame| draw(frame, &awaiting_fix(), &mut ui)).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains(" gps "), "{rendered}");
        assert!(rendered.contains("sentences"), "the modal's rows: {rendered}");
        assert!(rendered.contains("esc close"), "{rendered}");
        assert!(!rendered.contains("skip this wait"), "the pop-up is under it: {rendered}");
    }

    #[test]
    fn view_shows_counts_chart_and_blocks_when_terminal_is_wide() {
        let mut terminal = Terminal::new(TestBackend::new(120, 38)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("unique APs"), "{rendered}");
        assert!(rendered.contains("unique BLE"), "{rendered}");
        assert!(rendered.contains("1 2 3 4 5 6 7 8 9 0 1 2 3"), "the channel labels: {rendered}");
        assert!(rendered.contains("wifi sightings per channel"), "{rendered}");
        assert!(rendered.contains(" N7 "), "the blocks: {rendered}");
        assert!(rendered.contains("no heartbeat"), "the one node nothing can be sent to yet");
    }

    #[test]
    fn view_shows_big_counts_chart_and_two_blocks_per_row_when_terminal_is_58_by_26() {
        let mut snapshot = fleet_of(6);
        snapshot.unique_wifi_aps = 95;
        snapshot.unique_ble_aps = 20;
        let mut terminal = Terminal::new(TestBackend::new(58, 26)).expect("test backend");
        terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        let counts = rendered.lines().find(|l| l.contains("unique APs")).expect("the counts");
        assert!(counts.contains("unique BLE"), "side by side: {rendered}");
        assert!(rendered.lines().any(|l| l.contains(" 36 ")), "a band label: {rendered}");
        let blocks = rendered.lines().find(|l| l.contains(" N1 ")).expect("the first row");
        assert!(blocks.contains(" N2 "), "two blocks a row: {rendered}");
        assert!(!rendered.contains(" N5 "), "the third row is below the fold: {rendered}");
        assert!(rendered.contains("N1 C5 57:7E"), "the detail line: {rendered}");
    }
}
