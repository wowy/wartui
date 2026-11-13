//! The last panic's text, kept across the reset for the next life to send the P4.
//!
//! The panic handler resets at once, so without this a panic reads only as a `Software` reset
//! and a phase. Kept the way `wartui_firmware::phase` keeps its marker: persistent RTC fast
//! memory, `portable-atomic` atomics rather than a `static mut`, and a magic word written last,
//! believed only when the reset reason says the RTC domain survived.
//!
//! **Every fault reaches the panic handler.** On the C6, esp-hal's `ExceptionHandler` turns
//! each trap into a `panic!`: an access fault, an illegal instruction, and the breakpoint its
//! stack-guard watchpoint raises when the main stack's guard word is written. esp-rtos sets the
//! same watchpoint on each thread's guard at every switch. An overflow that jumps past the guard
//! word without writing it is not caught, and a watchdog reset skips the handler; the phase
//! marker covers those.
//!
//! The handler writes into the atomics directly, with no buffer, since after a stack overflow it
//! runs past the stack's end. It reports the text once: the record is cleared when the P4's
//! first hello arrives, by which time its lines are queued, so a reset before the link comes up
//! keeps it for the next life.

use core::fmt::{self, Write};
use core::sync::atomic::AtomicBool;

use portable_atomic::{AtomicU32, Ordering};
use wartui_proto::link::LogStr;
use wartui_proto::radio_link::Up;
use wartui_proto::reset::ResetCause;

use crate::link::Shared;

/// Bytes of text kept: three lines to the P4 after their prefixes.
const CAPACITY: usize = 240;

const WORDS: usize = CAPACITY / 4;

/// The text, four bytes a word, little end first.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static TEXT: [AtomicU32; WORDS] = [const { AtomicU32::new(0) }; WORDS];

/// Bytes of [`TEXT`] in use.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static LEN: AtomicU32 = AtomicU32::new(0);

/// Says that [`TEXT`] and [`LEN`] were written whole by this build's panic handler. Reads as
/// `pan1` in a memory dump.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static VALID: AtomicU32 = AtomicU32::new(0);

const MAGIC: u32 = 0x7061_6e31;

/// Set by the first panic, so a panic inside the handler resets rather than recursing.
static PANICKING: AtomicBool = AtomicBool::new(false);

/// What the first line to the P4 starts with, and every line after it.
const FIRST: &str = "C6 panicked: ";
const MORE: &str = "C6 panicked, cont.: ";

/// Record `info` as `{location}: {message}`, cut at [`CAPACITY`] bytes. Never allocates,
/// touches no peripheral and cannot panic; a second call while the first runs returns at once.
pub fn record(info: &core::panic::PanicInfo) {
    if PANICKING.swap(true, core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    VALID.store(0, Ordering::Relaxed);
    let mut out = Store { len: 0, full: false };
    let _ = match info.location() {
        Some(at) => {
            write!(
                out,
                "{}:{}:{}: {}",
                short_path(at.file()),
                at.line(),
                at.column(),
                info.message()
            )
        }
        None => write!(out, "{}", info.message()),
    };
    LEN.store(out.len as u32, Ordering::Relaxed);
    VALID.store(MAGIC, Ordering::Release);
}

/// Queue the last life's panic for the P4, if this reset kept one, split over as many `Error`
/// lines as it takes. The lines wait behind the hello exchange.
pub fn report(cause: ResetCause, shared: &Shared) {
    if matches!(cause, ResetCause::PowerOn) || VALID.load(Ordering::Acquire) != MAGIC {
        return;
    }
    let mut bytes = [0; CAPACITY];
    let len = (LEN.load(Ordering::Relaxed) as usize).min(CAPACITY);
    for (i, byte) in bytes.iter_mut().enumerate().take(len) {
        *byte = (TEXT[i / 4].load(Ordering::Relaxed) >> (i % 4 * 8)) as u8;
    }
    let bytes = &bytes[..len];
    // The handler cuts only at a character's end, so a failure here is corruption: keep what
    // decodes.
    let mut rest = match core::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => core::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap_or_default(),
    };
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

/// Forget the record, once its lines are queued and the P4 is there to read them.
pub fn clear() {
    VALID.store(0, Ordering::Relaxed);
}

/// A dependency's path from its crate's directory on: the registry's absolute prefix would take
/// half the record.
fn short_path(file: &str) -> &str {
    match file.split_once("/registry/src/") {
        Some((_, rest)) => rest.split_once('/').map_or(rest, |(_, path)| path),
        None => file,
    }
}

/// Writes straight into [`TEXT`], dropping whatever does not fit rather than failing, and cutting
/// only at a character's end.
struct Store {
    len: usize,
    full: bool,
}

impl Write for Store {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let mut utf8 = [0; 4];
            let encoded = c.encode_utf8(&mut utf8).as_bytes();
            if self.full || self.len + encoded.len() > CAPACITY {
                self.full = true;
                return Ok(());
            }
            for &byte in encoded {
                if let Some(word) = TEXT.get(self.len / 4) {
                    let shift = self.len % 4 * 8;
                    let kept = word.load(Ordering::Relaxed) & !(0xff << shift);
                    word.store(kept | u32::from(byte) << shift, Ordering::Relaxed);
                }
                self.len += 1;
            }
        }
        Ok(())
    }
}
