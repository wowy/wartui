//! One text form for an address: uppercase hex, colon-separated.
//!
//! The host, both firmwares' logs and the WiGLE export all print it, so an address greps
//! across every one of them, and changing it changes the export. What it prints pastes
//! back in as `--bridge`.
//!
//! The form is a `Display` wrapper rather than a `String` because this crate is `no_std`
//! and allocation-free, the same as [`crate::beacon::RcoiText`].

use core::fmt;

/// A MAC address.
pub type Mac = [u8; 6];

/// Octets in the shared form, for a whole address or part of one.
pub struct Octets<'a>(pub &'a [u8]);

impl fmt::Display for Octets<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
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

/// Read an address in the shared form: six colon-separated two-digit hex pairs, in
/// either case. An ESP32's USB serial number is written the same way.
#[must_use]
pub fn parse(text: &str) -> Option<Mac> {
    let mut mac = [0u8; 6];
    let mut octets = text.split(':');
    for slot in &mut mac {
        let octet = octets.next()?;
        if octet.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(octet, 16).ok()?;
    }
    octets.next().is_none().then_some(mac)
}
