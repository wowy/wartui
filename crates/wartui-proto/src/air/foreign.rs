/// The vendor's frame preamble.
pub const VENDOR_MAGIC: [u8; 4] = *b"ENOW";

/// The vendor's `MSG_ADMIN` type byte.
const VENDOR_ADMIN: u8 = 5;

/// Offset of the type byte in a vendor frame.
const VENDOR_OFF_TYPE: usize = 4;

/// What kind of vendor frame this is, to the small extent it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreign {
    /// Another host is assigning channels nearby. Worth its own count: it tells a
    /// neighboring fleet apart from a second host contending for this one.
    Admin,
    /// A vendor node's heartbeat or sighting, or the encrypted-pairing frames only a
    /// node with encryption switched on sends.
    Node,
}

/// Classify a frame that is not ours, or `None` if it is not the vendor's
/// either.
#[must_use]
pub fn classify(buf: &[u8]) -> Option<Foreign> {
    if buf.len() <= VENDOR_OFF_TYPE || buf[..4] != VENDOR_MAGIC {
        return None;
    }
    Some(if buf[VENDOR_OFF_TYPE] == VENDOR_ADMIN { Foreign::Admin } else { Foreign::Node })
}
