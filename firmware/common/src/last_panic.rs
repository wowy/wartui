//! The last panic's text, kept across the reset for the next life to report.
//!
//! A firmware's panic handler resets at once, so without this a panic reads only as a `Software`
//! reset. Kept the way [`crate::phase`] keeps its marker: persistent RTC fast memory,
//! `portable-atomic` atomics rather than a `static mut`, and a magic word written last, believed
//! only when the reset reason says the RTC domain survived. A power-on leaves the memory
//! undefined, so [`take`] refuses one.
//!
//! **Every fault reaches the panic handler.** On these RISC-V chips esp-hal's exception handler
//! turns each trap into a `panic!`: an access fault, an illegal instruction, and the breakpoint
//! its stack-guard watchpoint raises when a guarded stack's guard word is written. An overflow
//! that jumps past the guard word without writing it is not caught, and a watchdog reset skips
//! the handler; a bridge's phase marker covers those.
//!
//! [`record`] writes into the atomics directly, with no buffer, since after a stack overflow the
//! handler runs past the stack's end. It records once: a panic inside the handler, or a second
//! core's panic while the first records, returns at once, and its caller resets. [`VALID`] is
//! cleared first and set last, so a reset mid-record leaves no record rather than a torn one.
//!
//! **Clearing after reporting is what keeps a panic from being reported twice.** Any reset but a
//! power-on keeps the record, a brownout that does not zero RTC memory included, so a record
//! never cleared is reported again after every such reset. Each firmware calls [`clear`] once
//! the text has reached somewhere that keeps it.

use core::fmt::{self, Write};
use core::sync::atomic::AtomicBool;

use portable_atomic::{AtomicU32, Ordering};
use wartui_proto::bus::{PANIC_TEXT_MAX, PanicStr};
use wartui_proto::reset::ResetCause;

/// Bytes of text kept: what a node's panic record carries.
pub const CAPACITY: usize = PANIC_TEXT_MAX;

const WORDS: usize = CAPACITY / 4;

const _: () = assert!(CAPACITY.is_multiple_of(4));

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

/// Record `info` as `{location}: {message}`, cut at [`CAPACITY`] bytes, and return whether this
/// call recorded it. Never allocates, touches no peripheral and cannot panic; a second call
/// while the first runs, or after it, returns `false` at once.
pub fn record(info: &core::panic::PanicInfo) -> bool {
    if PANICKING.swap(true, core::sync::atomic::Ordering::Relaxed) {
        return false;
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
    true
}

/// The last life's panic text, if this reset kept one. Leaves the record in place: [`clear`] it
/// once the text is somewhere that keeps it.
#[must_use]
pub fn take(cause: ResetCause) -> Option<PanicStr> {
    if matches!(cause, ResetCause::PowerOn) || VALID.load(Ordering::Acquire) != MAGIC {
        return None;
    }
    let mut bytes = [0; CAPACITY];
    let len = (LEN.load(Ordering::Relaxed) as usize).min(CAPACITY);
    for (i, byte) in bytes.iter_mut().enumerate().take(len) {
        *byte = (TEXT[i / 4].load(Ordering::Relaxed) >> (i % 4 * 8)) as u8;
    }
    let bytes = &bytes[..len];
    // The handler cuts only at a character's end, so a failure here is corruption: keep what
    // decodes.
    let text = match core::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => core::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap_or_default(),
    };
    let mut out = PanicStr::new();
    // At most `CAPACITY` bytes, which is the string's.
    let _ = out.push_str(text);
    Some(out)
}

/// Forget the record.
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
