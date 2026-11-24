//! The last panic's text, sent to the P4 as `Error` lines.
//!
//! `wartui_firmware::last_panic` keeps the text across the reset; this splits it into the lines
//! the radio link carries. It reports the text once: the record is cleared when the P4's first
//! hello arrives, by which time its lines are queued, so a reset before the link comes up keeps
//! it for the next life.
//!
//! **Every fault reaches the panic handler.** On the C6, esp-hal's `ExceptionHandler` turns
//! each trap into a `panic!`, and esp-rtos sets the stack-guard watchpoint on each thread's
//! guard at every switch. A watchdog reset skips the handler; the phase marker covers that.

use wartui_proto::link::LogStr;
use wartui_proto::radio_link::Up;
use wartui_proto::reset::ResetCause;

pub use wartui_firmware::last_panic::{clear, record};

use crate::link::Shared;

/// What the first line to the P4 starts with, and every line after it.
const FIRST: &str = wartui_proto::link::RADIO_PANICKED;
const MORE: &str = "C6 panicked, cont.: ";

/// Queue the last life's panic for the P4, if this reset kept one, split over as many `Error`
/// lines as it takes. The lines wait behind the hello exchange.
pub fn report(cause: ResetCause, shared: &Shared) {
    let Some(text) = wartui_firmware::last_panic::take(cause) else { return };
    let mut rest = text.as_str();
    let mut prefix = FIRST;
    loop {
        let mut line = LogStr::new();
        let _ = line.push_str(prefix);
        let mut taken = 0;
        for c in rest.chars() {
            if line.push(c).is_err() {
                break;
            }
            taken += c.len_utf8();
        }
        shared.push(&Up::Error { message: line });
        rest = rest.get(taken..).unwrap_or_default();
        if rest.is_empty() || taken == 0 {
            break;
        }
        prefix = MORE;
    }
}
