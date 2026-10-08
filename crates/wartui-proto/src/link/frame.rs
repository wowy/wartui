use serde::{Serialize, de::DeserializeOwned};

use super::{LINK_PROTO_VERSION, MAX_ESPNOW_PAYLOAD, MAX_FRAME, PANEL_ROWS};

// A frame is the version byte, the payload and the CRC, plus COBS overhead (one byte
// per 254 and a leading marker) and the terminator. Checked here so a payload that
// outgrew the buffer is a build error, not a silent truncation.
const MAX_BODY: usize = MAX_FRAME - 8;
const _: () = assert!(MAX_ESPNOW_PAYLOAD + 32 < MAX_BODY);
// The other frame worth checking. A panel push carries every row every time, so its
// worst case is fixed: `PANEL_ROWS` lines of a full `ShortStr`, each with a severity
// byte and a length prefix, plus the variant index and the vector's length. A panel that
// grew a row or a wider `ShortStr` is then a build error, not a bridge silently losing
// the bottom of its screen.
const _: () = assert!(
    PANEL_ROWS * (32 + 2) + 2 < MAX_BODY,
    "a whole-panel push must fit one frame, or a bridge would silently lose rows"
);

/// Why a frame could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The output buffer was too small for the encoded frame.
    BufferTooSmall,
    /// COBS decoding failed; the frame was corrupt.
    Corrupt,
    /// The frame was shorter than a version byte plus a CRC.
    TooShort,
    /// CRC mismatch — a partial write, or line noise after a reset.
    BadChecksum {
        /// What the frame claimed.
        expected: u16,
        /// What the bytes actually hash to.
        actual: u16,
    },
    /// The peer speaks a different revision of this protocol.
    VersionMismatch {
        /// What this build speaks.
        ours: u8,
        /// What arrived.
        theirs: u8,
    },
    /// The body was not a valid message.
    Malformed,
}

impl core::fmt::Display for LinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BufferTooSmall => f.write_str("buffer too small for encoded frame"),
            Self::Corrupt => f.write_str("COBS decode failed"),
            Self::TooShort => f.write_str("frame shorter than its own header"),
            Self::BadChecksum { expected, actual } => {
                write!(f, "checksum mismatch: expected {expected:#06x}, got {actual:#06x}")
            }
            Self::VersionMismatch { ours, theirs } => {
                write!(f, "link protocol mismatch: we speak v{ours}, peer speaks v{theirs}")
            }
            Self::Malformed => f.write_str("frame body was not a valid message"),
        }
    }
}

impl core::error::Error for LinkError {}

/// CRC-16/CCITT-FALSE: polynomial `0x1021`, initial value `0xFFFF`, no
/// reflection and no final XOR.
///
/// Written here rather than pulled in as a dependency: it is a dozen lines, and the
/// firmware's dependency budget is worth defending.
#[must_use]
pub const fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    let mut i = 0;
    while i < data.len() {
        crc ^= (data[i] as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// Serialize `msg` into `out` as a complete frame, terminator included.
///
/// Returns how many bytes of `out` were used.
///
/// # Errors
/// [`LinkError::BufferTooSmall`] if `out` cannot hold the encoded frame.
pub fn encode_frame<T: Serialize>(msg: &T, out: &mut [u8]) -> Result<usize, LinkError> {
    let mut body = [0u8; MAX_BODY];
    body[0] = LINK_PROTO_VERSION;
    let used =
        postcard::to_slice(msg, &mut body[1..]).map_err(|_| LinkError::BufferTooSmall)?.len();
    let end = 1 + used;
    if end + 2 > MAX_BODY {
        return Err(LinkError::BufferTooSmall);
    }
    let crc = crc16(&body[..end]);
    body[end..end + 2].copy_from_slice(&crc.to_le_bytes());

    let encoded = cobs::try_encode(&body[..end + 2], out).map_err(|_| LinkError::BufferTooSmall)?;
    // COBS output never contains a zero, so the terminator is unambiguous.
    *out.get_mut(encoded).ok_or(LinkError::BufferTooSmall)? = 0x00;
    Ok(encoded + 1)
}

/// Decode a COBS-encoded frame body, with the terminating zero already removed.
///
/// # Errors
/// See [`LinkError`].
pub fn decode_frame<T: DeserializeOwned>(frame: &mut [u8]) -> Result<T, LinkError> {
    let len = cobs::decode_in_place(frame).map_err(|_| LinkError::Corrupt)?;
    if len < 3 {
        return Err(LinkError::TooShort);
    }
    let body = &frame[..len];
    let (payload, checksum) = body.split_at(len - 2);
    let expected = u16::from_le_bytes([checksum[0], checksum[1]]);
    let actual = crc16(payload);
    if expected != actual {
        return Err(LinkError::BadChecksum { expected, actual });
    }
    if payload[0] != LINK_PROTO_VERSION {
        return Err(LinkError::VersionMismatch { ours: LINK_PROTO_VERSION, theirs: payload[0] });
    }
    postcard::from_bytes(&payload[1..]).map_err(|_| LinkError::Malformed)
}

/// Reassembles frames from a byte stream.
///
/// Both ends use this. A zero byte ends a frame, so the accumulator recovers on its own
/// from a reset banner, a half-written frame or an unplugged cable. The junk is
/// discarded at the next terminator, and the stream carries on. Without a terminator of
/// the sender's own, that is the next frame's, so the bridge writes one at boot (see
/// [`crate::outbox`]).
#[derive(Debug)]
pub struct FrameAccumulator<const N: usize = MAX_FRAME> {
    buf: [u8; N],
    len: usize,
    overflowed: bool,
}

impl<const N: usize> Default for FrameAccumulator<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FrameAccumulator<N> {
    /// An empty accumulator.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0u8; N], len: 0, overflowed: false }
    }

    /// Discard any partial frame.
    pub const fn reset(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }

    /// Feed one byte.
    ///
    /// Returns the raw COBS-encoded frame when a terminator completes one; pass it to
    /// [`decode_frame`]. Empty frames and frames that overran the buffer yield `None`,
    /// having resynchronized.
    pub fn push(&mut self, byte: u8) -> Option<&mut [u8]> {
        if byte != 0x00 {
            if self.len < N {
                self.buf[self.len] = byte;
                self.len += 1;
            } else {
                self.overflowed = true;
            }
            return None;
        }

        let len = self.len;
        let overflowed = self.overflowed;
        self.reset();
        if overflowed || len == 0 {
            // A run of zeros, or a frame too big to be ours. Either way the next
            // terminator gives a clean start.
            return None;
        }
        Some(&mut self.buf[..len])
    }
}
