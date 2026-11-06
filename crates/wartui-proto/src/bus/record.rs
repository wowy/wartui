//! The records a transfer's payload carries, and how several share one transfer.
//!
//! A record is `[type: u8][len: u16 LE][body]`. Records follow one another to the payload's
//! `len`. A record never spans transfers: the longest fits one, so [`Packer`] refuses rather
//! than splits.
//!
//! | Type | [`RecordType`] | Body |
//! |---|---|---|
//! | 1 | [`RecordType::NodeFrame`] | one node frame verbatim, as it would go on the air ([`crate::air`]) |
//!
//! The record layer is fixed; the table grows. A receiver skips a type it does not know.

use super::{HEADER_LEN, Header, PAYLOAD_MAX, TRANSFER_LEN, seal};

/// Bytes of type and length ahead of each record's body.
pub const RECORD_HEADER_LEN: usize = 3;
/// The longest record body: a whole payload less one record header.
pub const RECORD_MAX: usize = PAYLOAD_MAX - RECORD_HEADER_LEN;

/// What a record's body holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// One node frame verbatim.
    NodeFrame = 1,
}

impl RecordType {
    /// The type a record's byte names, if this build knows it.
    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::NodeFrame),
            _ => None,
        }
    }
}

/// One record from a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// The type byte as sent, known to this build or not.
    pub kind: u8,
    /// The body.
    pub body: &'a [u8],
}

impl Record<'_> {
    /// The type, if this build knows it.
    #[must_use]
    pub const fn record_type(&self) -> Option<RecordType> {
        RecordType::from_u8(self.kind)
    }
}

/// Packs records into a transfer buffer, then seals it.
#[derive(Debug)]
pub struct Packer<'a> {
    buf: &'a mut [u8; TRANSFER_LEN],
    len: usize,
}

impl<'a> Packer<'a> {
    /// A packer writing from the start of `buf`'s payload.
    pub fn new(buf: &'a mut [u8; TRANSFER_LEN]) -> Self {
        Self { buf, len: 0 }
    }

    /// Whether a record of `body_len` bytes fits what is left.
    #[must_use]
    pub const fn fits(&self, body_len: usize) -> bool {
        self.len + RECORD_HEADER_LEN + body_len <= PAYLOAD_MAX
    }

    /// Appends one record, or returns `false` and changes nothing when it does not fit.
    pub fn push(&mut self, kind: RecordType, body: &[u8]) -> bool {
        if !self.fits(body.len()) {
            return false;
        }
        let at = HEADER_LEN + self.len;
        self.buf[at] = kind as u8;
        // Fits: no body that fits is longer than RECORD_MAX.
        self.buf[at + 1..at + RECORD_HEADER_LEN]
            .copy_from_slice(&(body.len() as u16).to_le_bytes());
        self.buf[at + RECORD_HEADER_LEN..at + RECORD_HEADER_LEN + body.len()].copy_from_slice(body);
        self.len += RECORD_HEADER_LEN + body.len();
        true
    }

    /// Payload bytes packed so far, the `len` to [`seal`] the transfer with.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is packed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Writes the header and CRC around what is packed.
    pub fn seal(self, header: Header) {
        seal(self.buf, header, self.len);
    }
}

/// The records in a payload, in order. A record whose header or body runs past the payload
/// ends the iteration.
#[must_use]
pub const fn records(payload: &[u8]) -> Records<'_> {
    Records { rest: payload }
}

/// The iterator [`records`] returns.
#[derive(Debug, Clone)]
pub struct Records<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Records<'a> {
    type Item = Record<'a>;

    fn next(&mut self) -> Option<Record<'a>> {
        let (&[kind, lo, hi], tail) = self.rest.split_first_chunk::<RECORD_HEADER_LEN>()?;
        let len = usize::from(u16::from_le_bytes([lo, hi]));
        if len > tail.len() {
            self.rest = &[];
            return None;
        }
        let (body, next) = tail.split_at(len);
        self.rest = next;
        Some(Record { kind, body })
    }
}
