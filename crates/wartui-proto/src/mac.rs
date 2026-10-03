//! One text form for an address: uppercase hex, colon-separated.
//!
//! The host, the node's logs, the bridge's panel and the WiGLE export all print it, so an
//! address greps across every one of them, and changing it changes the export. What it
//! prints pastes back in as `--bridge`.
//!
//! The form is a `Display` wrapper rather than a `String` because this crate is `no_std`
//! and allocation-free, the same as [`crate::beacon::RcoiText`].

use core::fmt::{self, Write};

/// A MAC address.
pub type Mac = [u8; 6];

/// The longest text an [`Octets`] prints: six pairs and five colons.
const FULL_LEN: usize = 6 * 3 - 1;

/// Octets in the shared form, made by [`full`], [`short`] or [`octets`].
pub struct Octets<'a>(&'a [u8]);

impl fmt::Display for Octets<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Written whole and then padded, so width, fill and alignment apply to the
        // address as one piece.
        let mut text = heapless::String::<FULL_LEN>::new();
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                text.write_char(':')?;
            }
            write!(text, "{byte:02X}")?;
        }
        f.pad(&text)
    }
}

/// The whole address, `02:00:5E:10:57:84`.
#[must_use]
pub fn full(mac: &Mac) -> Octets<'_> {
    Octets(mac)
}

/// The last two octets, `57:84`: how the boards are told apart (see AGENTS.md).
#[must_use]
pub fn short(mac: &Mac) -> Octets<'_> {
    Octets(&mac[4..])
}

/// Part of an address, such as the tail an operator named a board by.
///
/// Prints at most the first six octets. No address is longer, and the cap is what
/// lets the text fit a fixed buffer rather than an allocation.
#[must_use]
pub fn octets(bytes: &[u8]) -> Octets<'_> {
    Octets(&bytes[..bytes.len().min(6)])
}

/// Read an address in the shared form: six colon-separated [`pair`]s. An ESP32's USB
/// serial number is written the same way.
#[must_use]
pub fn parse(text: &str) -> Option<Mac> {
    let mut mac = [0u8; 6];
    let mut octets = text.split(':');
    for slot in &mut mac {
        *slot = pair(octets.next()?)?;
    }
    octets.next().is_none().then_some(mac)
}

/// One octet written as exactly two hex digits, in either case.
///
/// Checked by hand because `from_str_radix` also takes a sign, and `+8` is not a pair.
#[must_use]
pub fn pair(text: &str) -> Option<u8> {
    if text.len() != 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(text, 16).ok()
}
