//! Node → P4: the text of the node's last panic, sent once after the reset it caused.
//!
//! A panic resets the node at once, so without this the host sees only a `Software` reset in
//! the next heartbeat. The node keeps the text across the reset (`wartui_firmware::last_panic`)
//! and delivers it once a P4 is clocking its slot.
//!
//! The body is the node's MAC, then the text as UTF-8, `0..=`[`PANIC_TEXT_MAX`] bytes:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 6 | the node's MAC |
//! | 6 | 0 to 240 | the panic's `{file}:{line}:{column}: {message}`, cut at a character's end |
//!
//! The MAC names the node, as a heartbeat's does, so the host files the text under the node
//! that panicked rather than whichever the slot last named. Up type 4 is outside the bus's
//! frozen core: the layout changes freely between builds.

use super::DecodeError;
use crate::mac::Mac;

/// The most text a [`PanicMsg`] carries, in bytes.
pub const PANIC_TEXT_MAX: usize = 240;

/// The shortest [`PanicMsg`] body: the MAC, with no text.
pub const PANIC_MIN: usize = 6;

/// The longest [`PanicMsg`] body.
pub const PANIC_MAX: usize = PANIC_MIN + PANIC_TEXT_MAX;

/// Panic text as the node keeps it and the P4's SD log carries it.
pub type PanicStr = heapless::String<PANIC_TEXT_MAX>;

const _: () = assert!(PANIC_MAX <= crate::link::MAX_NODE_PAYLOAD);

/// Node → P4: why the node's last life ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanicMsg<'a> {
    /// The node's station MAC, the address the host knows it by.
    pub mac: Mac,
    /// The panic's text.
    pub text: &'a str,
}

impl<'a> PanicMsg<'a> {
    /// Decode a body.
    ///
    /// # Errors
    /// [`DecodeError::TooShort`] under the MAC, [`DecodeError::BadLength`] past
    /// [`PANIC_MAX`], and [`DecodeError::NotUtf8`] for text that is not UTF-8.
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        if body.len() < PANIC_MIN {
            return Err(DecodeError::TooShort { need: PANIC_MIN, got: body.len() });
        }
        if body.len() > PANIC_MAX {
            return Err(DecodeError::BadLength { need: PANIC_MAX, got: body.len() });
        }
        let (mac, text) = body.split_at(PANIC_MIN);
        let text = core::str::from_utf8(text).map_err(|_| DecodeError::NotUtf8)?;
        let mut out = [0; 6];
        out.copy_from_slice(mac);
        Ok(Self { mac: out, text })
    }

    /// Write the body into `out` and return its length, or `None` when `out` is too short or
    /// the text is past [`PANIC_TEXT_MAX`].
    #[must_use]
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        if self.text.len() > PANIC_TEXT_MAX {
            return None;
        }
        let len = PANIC_MIN + self.text.len();
        let out = out.get_mut(..len)?;
        out[..PANIC_MIN].copy_from_slice(&self.mac);
        out[PANIC_MIN..].copy_from_slice(self.text.as_bytes());
        Some(len)
    }
}
