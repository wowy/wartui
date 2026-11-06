//! The messages a [`Kind::Messages`](super::Kind::Messages) packet carries, and how
//! several share one packet.
//!
//! Not frozen. Both firmwares are built from one tree, so these are postcard, like the host
//! link, and change whenever the work needs. [`LINK_REVISION`] names the encoding, and the
//! hello carries it so a P4 never bridges with a C6 that encodes them differently.
//!
//! Today they carry only what the pair needs: the P4's poll for the C6's counters, the
//! counters, and the C6's lines for the host. The C6 holds the link for a future host link
//! over Bluetooth.
//!
//! A payload is records of `[len: u16 LE][postcard bytes]`, ending at a zero `len` or the
//! payload's end. A record never spans packets: the largest message fits one, so the
//! packer refuses rather than splits. A queued record that does not fit is [`Held`] and
//! leads the next packet.

use heapless::Vec;
use serde::{Deserialize, Serialize};

use crate::link::{LogLevel, LogStr, QueueStat};

/// The revision of the [`Up`] and [`Down`] encoding, which [`Hello`](super::Hello)
/// carries.
///
/// Not frozen. Bump it whenever this module's types change shape; `radio_link_revision_golden_bytes` in `tests/radio_link.rs` fails until the bump.
pub const LINK_REVISION: u16 = 1;

/// P4 to C6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Down {
    /// Ask for an [`Up::RadioStatus`].
    GetRadioStatus,
}

/// C6 to P4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Up {
    /// Reply to [`Down::GetRadioStatus`].
    RadioStatus(RadioStatus),
    /// A line for the host: a `Log` at `level`, or an `Error` without one.
    Log {
        /// The severity, or `None` for an error.
        level: Option<LogLevel>,
        /// The text.
        message: LogStr,
    },
}

/// The C6's counters, which the P4 reports beside its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioStatus {
    /// Bytes free in the C6's heap.
    pub heap_free: u32,
    /// Milliseconds since the C6's life started.
    pub uptime_ms: u32,
    /// The C6's bounded queues.
    pub queues: Vec<QueueStat, 4>,
    /// Packets from the P4 that failed their CRC.
    pub crc_fail: u32,
    /// Packets from the P4 without [`super::MAGIC`].
    pub bad_magic: u32,
    /// Breaks in the P4's seq.
    pub seq_gap: u32,
    /// Line errors on this end's UART: framing, parity, glitch or FIFO overflow.
    pub uart_errors: u32,
}

/// Packs encoded records into a packet's payload.
#[derive(Debug)]
pub struct Packer<'a> {
    payload: &'a mut [u8],
    len: usize,
}

impl<'a> Packer<'a> {
    /// A packer writing from the start of `payload`.
    pub fn new(payload: &'a mut [u8]) -> Self {
        Self { payload, len: 0 }
    }

    /// Appends one record, or returns `false` and changes nothing when it does not fit.
    pub fn push_encoded(&mut self, record: &[u8]) -> bool {
        let end = self.len + PREFIX + record.len();
        let Ok(record_len) = u16::try_from(record.len()) else { return false };
        if end > self.payload.len() {
            return false;
        }
        self.payload[self.len..self.len + PREFIX].copy_from_slice(&record_len.to_le_bytes());
        self.payload[self.len + PREFIX..end].copy_from_slice(record);
        self.len = end;
        true
    }

    /// Bytes packed so far, the `len` to seal the packet with.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is packed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The longest record: a payload less its record's length prefix. A queue that holds none
/// longer always fits its record into an empty packet.
pub const RECORD_MAX: usize = super::PAYLOAD_MAX - PREFIX;

/// Bytes of length prefix ahead of each record.
const PREFIX: usize = 2;

/// A record taken from a queue that did not fit the last packet, sent first in the next.
#[derive(Debug)]
pub struct Held {
    buf: [u8; RECORD_MAX],
    len: usize,
}

impl Held {
    /// Nothing held. Const, so firmware builds it in a static.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; RECORD_MAX], len: 0 }
    }

    /// Packs the held record, then records from `next`, until one does not fit — it is then
    /// held for the next packet — or `next` has none.
    ///
    /// `next` copies one whole record into the buffer it is given and returns its length,
    /// or 0 when nothing is queued. `next` must read from a queue holding no record longer
    /// than [`RECORD_MAX`], so that one read is a whole record.
    pub fn pack(&mut self, packer: &mut Packer<'_>, mut next: impl FnMut(&mut [u8]) -> usize) {
        loop {
            if self.len == 0 {
                self.len = next(&mut self.buf);
                if self.len == 0 {
                    break;
                }
            }
            if !packer.push_encoded(&self.buf[..self.len]) {
                break;
            }
            self.len = 0;
        }
    }
}

impl Default for Held {
    fn default() -> Self {
        Self::new()
    }
}

/// The records in a payload, in order. A record whose `len` runs past the payload ends
/// the iteration.
pub fn records(payload: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = payload;
    core::iter::from_fn(move || {
        let (head, tail) = rest.split_first_chunk::<PREFIX>()?;
        let len = usize::from(u16::from_le_bytes(*head));
        if len == 0 || len > tail.len() {
            return None;
        }
        let (record, next) = tail.split_at(len);
        rest = next;
        Some(record)
    })
}

/// Encodes `msg` into `out` and returns its length.
///
/// # Errors
/// When `out` is too short.
pub fn encode_up(msg: &Up, out: &mut [u8]) -> Result<usize, postcard::Error> {
    postcard::to_slice(msg, out).map(|used| used.len())
}

/// Encodes `msg` into `out` and returns its length.
///
/// # Errors
/// When `out` is too short.
pub fn encode_down(msg: &Down, out: &mut [u8]) -> Result<usize, postcard::Error> {
    postcard::to_slice(msg, out).map(|used| used.len())
}

/// Decodes one record.
///
/// # Errors
/// When the record is not an [`Up`] this build can read.
pub fn decode_up(record: &[u8]) -> Result<Up, postcard::Error> {
    postcard::from_bytes(record)
}

/// Decodes one record.
///
/// # Errors
/// When the record is not a [`Down`] this build can read.
pub fn decode_down(record: &[u8]) -> Result<Down, postcard::Error> {
    postcard::from_bytes(record)
}
