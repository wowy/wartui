//! Keyboard input, read on its own thread rather than through an async stream.
//! crossterm's API is built for a blocking `poll` on stdin, and a thread keeps the
//! terminal off the tokio runtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

/// How long the input thread waits for a key before checking whether to stop. Long
/// enough not to spin, short enough that quitting feels instant.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// What the input thread forwards: a keypress, or a whole bracketed paste.
pub(super) enum Input {
    Key(KeyEvent),
    Paste(String),
}

/// Start the input thread. Clear the returned flag to stop it.
///
/// Fails when the thread cannot start. Without it no key reaches the view, not even
/// ctrl-c, which raw mode delivers as a key.
pub(super) fn spawn_input() -> Result<(mpsc::Receiver<Input>, Arc<AtomicBool>)> {
    let (tx, rx) = mpsc::channel(16);
    let running = Arc::new(AtomicBool::new(true));
    std::thread::Builder::new()
        .name("wartui-input".to_owned())
        .spawn({
            let running = Arc::clone(&running);
            move || {
                while running.load(Ordering::Relaxed) {
                    match event::poll(INPUT_POLL) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(_) => return,
                    }
                    let input = match event::read() {
                        Ok(TermEvent::Key(key)) => Input::Key(key),
                        Ok(TermEvent::Paste(text)) => Input::Paste(text),
                        _ => continue,
                    };
                    if tx.blocking_send(input).is_err() {
                        return;
                    }
                }
            }
        })
        .context("starting the keyboard thread")?;
    Ok((rx, running))
}

/// Whether `key` quits the view: `q`, `Esc` or ctrl-c.
pub(super) fn quits(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) || is_ctrl_c(key)
}

/// Whether `key` is ctrl-c. It quits even while a modal is open, where `q` and `Esc`
/// only close the modal.
pub(super) fn is_ctrl_c(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quits_returns_true_when_receiving_q_esc_or_ctrl_c() {
        assert!(quits(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!quits(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
    }
}
