//! The messages a [`Kind::Messages`](super::Kind::Messages) transfer carries, and how
//! several share one transfer.
//!
//! Not frozen. Both firmwares are built from one tree, so these are postcard, like the USB
//! link, and change whenever the work needs. [`LINK_REVISION`] names the encoding, and the
//! hello carries it so a P4 never bridges with a C6 that encodes them differently.
//!
//! The P4 relays the USB link's own messages whole, so the C6 runs the single-chip
//! bridge's command handling and nothing is translated in between. The only additions are
//! the C6's own status, which the P4 folds into what it reports to the host.
//!
//! A payload is records of `[len: u16 LE][postcard bytes]`, ending at a zero `len` or the
//! payload's end. A record never spans transfers: the largest message fits one, so the
//! packer refuses rather than splits. A queued record that does not fit is [`Held`] and
//! leads the next transfer.

use heapless::Vec;
use serde::{Deserialize, Serialize};

use crate::link::{BridgeToHost, HostToBridge, QueueStat};

/// The revision of the [`Up`] and [`Down`] encoding, which [`Hello`](super::Hello)
/// carries.
///
/// Not frozen. Bump it whenever this module's types or the link enums they carry change
/// shape; `spi_link_revision_golden_bytes` in `tests/spi.rs` fails until the bump.
pub const LINK_REVISION: u16 = 2;

/// P4 to C6.
// Unboxed for the reason `HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Down {
    /// A host command, relayed whole.
    Command(HostToBridge),
    /// Ask for an [`Up::RadioStatus`].
    GetRadioStatus,
}

/// C6 to P4.
// Unboxed for the reason `HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Up {
    /// A message for the host, relayed whole.
    Message(BridgeToHost),
    /// Reply to [`Down::GetRadioStatus`].
    RadioStatus(RadioStatus),
}

impl Up {
    /// Whether this record must survive a backlog of received frames: everything but a
    /// received frame, so a send result, a line or the C6's status is never evicted behind
    /// the traffic.
    #[must_use]
    pub const fn is_priority(&self) -> bool {
        !matches!(self, Up::Message(BridgeToHost::Rx { .. }))
    }
}

/// The C6's counters, which the P4 reports beside its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioStatus {
    /// Registered peers.
    pub peer_count: u8,
    /// Frames received and accepted to forward since boot.
    pub rx_count: u32,
    /// Received frames longer than [`crate::link::MAX_NODE_PAYLOAD`], dropped since boot.
    pub rx_oversize: u32,
    /// Bytes free in the C6's heap.
    pub heap_free: u32,
    /// Milliseconds since the C6's life started.
    pub uptime_ms: u32,
    /// The C6's bounded queues.
    pub queues: Vec<QueueStat, 4>,
    /// Transfers from the P4 that failed their CRC.
    pub crc_fail: u32,
    /// Transfers from the P4 without [`super::MAGIC`].
    pub bad_magic: u32,
    /// Breaks in the P4's seq.
    pub seq_gap: u32,
}

/// Packs encoded records into a transfer's payload.
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
        let end = self.len + 2 + record.len();
        let Ok(record_len) = u16::try_from(record.len()) else { return false };
        if end > self.payload.len() {
            return false;
        }
        self.payload[self.len..self.len + 2].copy_from_slice(&record_len.to_le_bytes());
        self.payload[self.len + 2..end].copy_from_slice(record);
        self.len = end;
        true
    }

    /// Bytes packed so far, the `len` to seal the transfer with.
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
/// longer always fits its record into an empty transfer.
pub const RECORD_MAX: usize = super::PAYLOAD_MAX - 2;

/// A record taken from a queue that did not fit the last transfer, sent first in the next.
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
    /// held for the next transfer — or `next` has none.
    ///
    /// `next` copies one whole record into the buffer it is given and returns its length,
    /// or 0 when nothing is queued.
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
        let (head, tail) = rest.split_first_chunk::<2>()?;
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
