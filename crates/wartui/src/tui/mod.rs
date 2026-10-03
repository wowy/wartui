//! The fleet view.
//!
//! See the operator's manual (`crates/wartui/README.md`) for a usage guide.
//!
//! Each frame is drawn from the latest [`Snapshot`], published four times a second.
//! Drawing per observation would back-pressure the link, since a busy fleet sends tens
//! of rows a second.

mod fleet;
mod footer;
mod format;
mod header;
mod input;
mod settings;
mod stream;
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
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_core::engine::{Command, Snapshot};

use fleet::draw_fleet;
use footer::{draw_footer, drop_line, fault_lines, faults};
use header::draw_header;
use input::{Input, is_ctrl_c, quits, spawn_input};
pub use settings::Settings;
use settings::draw_settings_modal;
use stream::draw_stream;
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
    let (mut inputs, running) = spawn_input();
    let mut ui = Ui { settings, ..Ui::default() };
    // Built before the loop, not inside the arm below. See `crate::Terminate`.
    let mut terminate = crate::Terminate::new();
    let outcome = loop {
        let current = snapshot.borrow_and_update().clone();
        ui.clamp(current.nodes.len());
        ui.poll_upload(&current);
        if let Err(e) = terminal.draw(|frame| draw(frame, &current, &mut ui)) {
            break Err(e).context("drawing the fleet view");
        }

        tokio::select! {
            input = inputs.recv() => match input {
                // ctrl-c always quits, modal or not.
                Some(Input::Key(key)) if is_ctrl_c(key) => break Ok(()),
                // The upload's confirm opens over everything, settings included, so
                // it answers first.
                Some(Input::Key(key)) if ui.upload.confirming() => ui.on_confirm_key(key, &current),
                // An open modal gets the key ahead of `quits()`, so `esc` and `q`
                // close the modal, not the view.
                Some(Input::Key(key)) if ui.modal.is_some() => {
                    ui.on_modal_key(key, &current, commands);
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
    let drops = u16::from(drop_line(&snapshot.counters).is_some());
    let upload = u16::from(ui.upload.status().is_some());
    let footer_height = 1 + drops + upload + u16::try_from(faults.len()).unwrap_or(u16::MAX);
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(footer_height),
    ])
    .areas(frame.area());

    draw_header(frame, header, snapshot);

    // Side by side when both fit, stacked otherwise. The fleet table needs 74
    // columns before its state column clips.
    let [fleet, stream] = if body.width >= 114 {
        Layout::horizontal([Constraint::Length(74), Constraint::Min(40)]).areas(body)
    } else {
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body)
    };
    draw_fleet(frame, fleet, snapshot, ui);
    draw_stream(frame, stream, snapshot);
    draw_footer(frame, footer, snapshot, ui, &faults);

    if let Some(modal) = &ui.modal {
        draw_settings_modal(frame, modal);
    }
    if let Some(lines) = ui.upload.confirm_lines() {
        draw_confirm_modal(frame, &lines);
    }
}

/// A `width` by `height` box centred in `area`, clipped to it when too big.
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
        for snapshot in [busy(), empty()] {
            for (width, height) in [(200, 50), (120, 30), (80, 24), (40, 10), (20, 5), (6, 3)] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("test backend");
                terminal.draw(|frame| draw(frame, &snapshot, &mut Ui::default())).expect("drawing");
            }
        }
    }

    #[test]
    fn view_shows_fleet_and_stream_side_by_side_when_terminal_is_wide() {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).expect("test backend");
        terminal.draw(|frame| draw(frame, &busy(), &mut Ui::default())).expect("drawing");
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("C5 57:84"), "the fleet table");
        assert!(rendered.contains("AA:BB:CC:DD:EE"), "and the observation stream");
        assert!(rendered.contains("4 of 5 alive"));
        assert!(rendered.contains("no heartbeat"), "the one node nothing can be sent to yet");
    }
}
