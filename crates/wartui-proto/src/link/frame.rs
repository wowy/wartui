use serde::{Serialize, de::DeserializeOwned};

use super::{
    BridgeToHost, GPS_FRAME, GPS_WRITE_MAX, HostToBridge, LINK_PROTO_VERSION,
    MAX_ESPNOW_V2_PAYLOAD, MAX_FRAME, MAX_HOST_FRAME, MAX_NODE_PAYLOAD, PANEL_ROWS,
};

// A frame is a body (the version byte, the payload and the CRC) plus COBS overhead (one
// byte per 254 and a leading marker, five for a body this size) and the terminator, so 8
// bytes cover the framing. Checked here so a payload that outgrew the buffer is a build
// error, not a silent truncation. Each `+ 32` below is room for the message's other
// fields, its variant index, and the body's version byte and CRC.
pub(crate) const MAX_BODY: usize = MAX_FRAME - 8;
const _: () = assert!(MAX_NODE_PAYLOAD + 32 < MAX_BODY);
// The same for the host-to-bridge direction, whose longest frame is a full v2 send.
// COBS adds one byte per 254, so 16 covers it with the terminator.
const MAX_HOST_BODY: usize = MAX_HOST_FRAME - 16;
const _: () = assert!(MAX_ESPNOW_V2_PAYLOAD + 32 < MAX_HOST_BODY);
// The GPS byte pipe, both ways, so a larger batch is a build error too.
const _: () = assert!(GPS_FRAME + 32 < MAX_BODY);
const _: () = assert!(GPS_WRITE_MAX + 32 < MAX_HOST_BODY);
// The other frame worth checking. A panel push carries every row every time, so its
// worst case is fixed: `PANEL_ROWS` lines of a full `ShortStr`, each with a severity
// byte and a length prefix, plus the variant index and the vector's length. A panel that
// grew a row or a wider `ShortStr` is then a build error, not a bridge silently losing
// the bottom of its screen.
const _: () = assert!(
    PANEL_ROWS * (32 + 2) + 2 < MAX_BODY,
    "a whole-panel push must fit one frame, or a bridge would silently lose rows"
);
// The two replies that grew with the split bridge, at their fullest.
const _: () = assert!(
    // variant index, `peer_count`, four `u32`s, then 8 queues of a tag and three `u32`s,
    // then the `Option` tag and nine `u32`s. Varint integers never exceed their fixed width
    // by more than a byte each.
    1 + 1 + 4 * 5 + 1 + 8 * (1 + 3 * 5) + 1 + 9 * 5 < MAX_BODY,
    "a Status with every queue and SPI stats must fit one frame"
);
const _: () = assert!(
    // Two `ShortStr`s behind their lengths and two MACs, the bridge's and the radio's.
    // `8 * 5` bounds eight varint `u32`s, against three today: `heap_free`, `uptime_ms` and
    // the radio's `uptime_ms`. `24` bounds the one-byte fields, against 13 today: the
    // variant index, `chip`, `proto_version`, `reset_cause`, `last_phase`, `gps`, the
    // panel's tag, `cols` and `rows`, and the radio's tag, `chip`, `reset_cause` and
    // `last_phase`. The slack lets either grow a little without moving the sum.
    2 * (32 + 1) + 2 * 6 + 8 * 5 + 24 < MAX_BODY,
    "a Ready with radio info must fit one frame"
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
/// Written here rather than pulled in as a dependency: a 512-byte table and about 25
/// lines cost less than another crate in the firmware's dependency budget. Table-driven,
/// one lookup per byte, because the bridge checksums every frame it forwards and a bitwise
/// loop over a full [`BridgeToHost::Rx`] cost ~310 µs on a C5, during which the main loop
/// pumps no USB.
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc = (crc << 8) ^ CRC16_TABLE[usize::from((crc >> 8) as u8 ^ byte)];
    }
    crc
}

/// [`crc16`]'s remainder for each value of the top byte, built at compile time. A
/// `static`, so every lookup reads the one table rather than a copy of it.
static CRC16_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut n = 0;
    while n < 256 {
        let mut crc = (n as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
};

/// Serialize a bridge-to-host `msg` into `out` as a complete frame, terminator included,
/// of up to [`MAX_FRAME`].
///
/// Each encoder takes one direction's enum, so a [`HostToBridge::SendEspNow`] can never be
/// encoded against the smaller bridge-to-host buffer.
///
/// Returns how many bytes of `out` were used.
///
/// # Errors
/// [`LinkError::BufferTooSmall`] if `out` cannot hold the encoded frame.
pub fn encode_frame(msg: &BridgeToHost, out: &mut [u8]) -> Result<usize, LinkError> {
    encode_frame_in::<_, MAX_BODY>(msg, out)
}

/// Serialize a host-to-bridge `msg` into `out` as a complete frame of up to
/// [`MAX_HOST_FRAME`], which a [`HostToBridge::SendEspNow`] with a full v2 payload needs.
///
/// Host only: its body buffer is three times [`encode_frame`]'s, on the stack.
///
/// # Errors
/// [`LinkError::BufferTooSmall`] if `out` cannot hold the encoded frame.
pub fn encode_host_frame(msg: &HostToBridge, out: &mut [u8]) -> Result<usize, LinkError> {
    encode_frame_in::<_, MAX_HOST_BODY>(msg, out)
}

fn encode_frame_in<T: Serialize, const B: usize>(
    msg: &T,
    out: &mut [u8],
) -> Result<usize, LinkError> {
    let mut body = [0u8; B];
    let len = encode_body(msg, &mut body)?;
    if out.is_empty() {
        return Err(LinkError::BufferTooSmall);
    }
    let mut stream = CobsStream::new();
    let written = stream.fill(&body[..len], out);
    if !stream.is_done() {
        return Err(LinkError::BufferTooSmall);
    }
    Ok(written)
}

/// Write a frame's body into `out`: the version byte, the serialized `msg` and its CRC,
/// little-endian. COBS and the terminator are the caller's.
///
/// Returns how many bytes of `out` were used.
///
/// # Errors
/// [`LinkError::BufferTooSmall`] if `out` cannot hold the body.
pub(crate) fn encode_body<T: Serialize>(msg: &T, out: &mut [u8]) -> Result<usize, LinkError> {
    let (version, payload) = out.split_first_mut().ok_or(LinkError::BufferTooSmall)?;
    *version = LINK_PROTO_VERSION;
    let used = postcard::to_slice(msg, payload).map_err(|_| LinkError::BufferTooSmall)?.len();
    let end = 1 + used;
    let crc = crc16(&out[..end]);
    out.get_mut(end..end + 2).ok_or(LinkError::BufferTooSmall)?.copy_from_slice(&crc.to_le_bytes());
    Ok(end + 2)
}

/// COBS-encodes a frame body a piece at a time, terminator included, producing exactly
/// what [`cobs::encode`] and a `0x00` would. Every frame this crate encodes goes through it.
///
/// It holds positions and never the body, which the caller passes on every call: the
/// outbox keeps the body in a ring slot that eviction may copy elsewhere, same bytes at
/// the same indices.
#[derive(Debug)]
pub(crate) struct CobsStream {
    /// The next body byte to read.
    at: usize,
    step: CobsStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CobsStep {
    /// A block starts at `at`, and its code byte is next.
    Code,
    /// `left` data bytes of the current block remain; `zero` when a zero in the body
    /// ends it, and is skipped rather than written.
    Data { left: u8, zero: bool },
    /// The terminator is next.
    Terminator,
    /// The terminator is written.
    Done,
}

/// The longest run of non-zero bytes one COBS block carries.
const COBS_RUN: usize = 254;

impl CobsStream {
    /// A stream at the start of a body.
    pub(crate) const fn new() -> Self {
        Self { at: 0, step: CobsStep::Code }
    }

    /// Write the next encoded bytes of `body` into `out`, as many as fit, and return how
    /// many. [`CobsStream::is_done`] says when the frame is complete; after that, `fill`
    /// writes nothing.
    ///
    /// `body` must be the same bytes on every call, and `out` must not be empty.
    pub(crate) fn fill(&mut self, body: &[u8], out: &mut [u8]) -> usize {
        debug_assert!(!out.is_empty(), "an empty `out` makes no progress");
        let mut n = 0;
        while n < out.len() {
            match self.step {
                CobsStep::Code => {
                    let rest = &body[self.at..];
                    let scan = &rest[..rest.len().min(COBS_RUN)];
                    let (len, zero) = match scan.iter().position(|&b| b == 0) {
                        Some(k) => (k, true),
                        None => (scan.len(), false),
                    };
                    // `len` is at most `COBS_RUN`, so the code is at most `0xFF`.
                    out[n] = len as u8 + 1;
                    n += 1;
                    self.step = CobsStep::Data { left: len as u8, zero };
                }
                CobsStep::Data { left, zero } => {
                    let take = usize::from(left).min(out.len() - n);
                    out[n..n + take].copy_from_slice(&body[self.at..self.at + take]);
                    n += take;
                    self.at += take;
                    // `take` is at most `left`, so it fits a `u8`.
                    let left = left - take as u8;
                    self.step = if left > 0 {
                        CobsStep::Data { left, zero }
                    } else if zero {
                        self.at += 1;
                        CobsStep::Code
                    } else if self.at == body.len() {
                        // A full run that ends the body opens no empty block behind
                        // it, which is how `cobs` encodes it.
                        CobsStep::Terminator
                    } else {
                        CobsStep::Code
                    };
                }
                CobsStep::Terminator => {
                    out[n] = 0x00;
                    n += 1;
                    self.step = CobsStep::Done;
                }
                CobsStep::Done => break,
            }
        }
        n
    }

    /// Whether the terminator has been written.
    pub(crate) const fn is_done(&self) -> bool {
        matches!(self.step, CobsStep::Done)
    }
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
/// [`crate::outbox`]), and the host leads each connection's first frame with one.
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

    /// Whether no frame is part-way in.
    ///
    /// A frame that overran the buffer is still arriving until its terminator, so it
    /// counts as part-way in. The bridge reads this to stay on the link while a host
    /// frame is crossing it ([`LinkBurst`](super::LinkBurst)).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0 && !self.overflowed
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

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::{CobsStream, MAX_BODY, MAX_FRAME};

    /// What `encode_frame` puts on the wire for `body`.
    fn reference(body: &[u8]) -> Vec<u8> {
        let mut out = std::vec![0; MAX_FRAME * 2];
        let len = cobs::encode(body, &mut out);
        out.truncate(len);
        out.push(0x00);
        out
    }

    /// Everything `CobsStream` produces for `body`, `chunk` bytes per call.
    fn streamed(body: &[u8], chunk: usize) -> Vec<u8> {
        let mut stream = CobsStream::new();
        let mut out = Vec::new();
        let mut buf = std::vec![0; chunk];
        while !stream.is_done() {
            let n = stream.fill(body, &mut buf);
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn assert_matches_cobs(body: &[u8]) {
        let expected = reference(body);
        for chunk in [1, 7, 64, MAX_FRAME * 2] {
            assert_eq!(streamed(body, chunk), expected, "{} bytes, {chunk} a call", body.len());
        }
    }

    fn nonzero(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 255) as u8 + 1).collect()
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_body_is_empty() {
        assert_matches_cobs(&[]);
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_body_is_one_zero() {
        assert_matches_cobs(&[0]);
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_body_is_all_zeros() {
        assert_matches_cobs(&[0; 300]);
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_body_has_no_zeros() {
        // Either side of one and two full 254-byte runs.
        for len in [253, 254, 255, 508, 509] {
            assert_matches_cobs(&nonzero(len));
        }
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_full_run_precedes_zero() {
        let mut body = nonzero(254);
        body.push(0);
        body.extend_from_slice(&nonzero(10));
        assert_matches_cobs(&body);
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_body_ends_in_zero() {
        let mut body = nonzero(20);
        body.push(0);
        assert_matches_cobs(&body);
    }

    #[test]
    fn cobs_stream_matches_cobs_encode_when_body_is_pseudo_random() {
        // An xorshift, with about one byte in eight a zero and runs of every length.
        let mut state: u32 = 0x2545_f491;
        let body: Vec<u8> = (0..MAX_BODY)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                if state.is_multiple_of(8) { 0 } else { (state >> 8) as u8 | 1 }
            })
            .collect();
        assert_matches_cobs(&body);
    }

    #[test]
    fn cobs_stream_returns_zero_when_terminator_is_written() {
        let body = nonzero(10);
        let mut stream = CobsStream::new();
        let mut buf = [0; 64];
        assert_eq!(stream.fill(&body, &mut buf), 12);
        assert_eq!(stream.fill(&body, &mut buf), 0);
        assert_eq!(stream.fill(&body, &mut buf), 0);
    }

    #[test]
    fn cobs_stream_reports_done_when_terminator_is_written() {
        let body = nonzero(10);
        let mut stream = CobsStream::new();
        // The code byte, the ten data bytes, and then the terminator alone.
        let mut buf = [0; 11];
        assert_eq!(stream.fill(&body, &mut buf), 11);
        assert!(!stream.is_done(), "the terminator is still owed");
        assert_eq!(stream.fill(&body, &mut buf), 1);
        assert!(stream.is_done());
    }
}
