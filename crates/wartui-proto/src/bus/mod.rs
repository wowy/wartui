//! The node bus: SPI between a P4 bridge, as master, and its nodes, as slaves.
//!
//! The P4 clocks every transfer. Each one is exactly [`TRANSFER_LEN`] bytes each way,
//! little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 2 | [`MAGIC`] |
//! | 2 | 1 | [`BUS_VERSION`] |
//! | 3 | 1 | flags: bit 0 [`MORE`], the sender has more queued after this transfer; other bits zero |
//! | 4 | 2 | seq: the sender's counter, +1 per transfer it arms, from 0 at boot |
//! | 6 | 2 | ack: the last seq received from the peer under a good CRC, 0 before the first |
//! | 8 | 2 | len: payload bytes, at most [`PAYLOAD_MAX`] |
//! | 10 | len | payload: records ([`Packer`], [`records`]); bytes past `len` are zero |
//! | [`CRC_OFFSET`] | 2 | CRC-16/CCITT-FALSE ([`crc16`]) over bytes `0..CRC_OFFSET` |
//!
//! **The length is fixed** because a slave is armed for one length before the master clocks,
//! and cannot learn a length from the transfer it is in. A fixed full-duplex transfer is the
//! path the P4–C6 bench proved error-free (`docs/p4-spi-link-findings.md`). [`TRANSFER_LEN`]
//! carries one record holding the longest node frame, [`MAX_NODE_PAYLOAD`], and is a multiple
//! of 4 for the DMA.
//!
//! **A node re-arms after every transfer, and asks for one through IRQ.**
//!
//! 1. The node arms its slave with the next transfer before the master clocks.
//! 2. The master clocks [`TRANSFER_LEN`] bytes with the node's CS low.
//! 3. CS rises, and the node arms the next transfer at once.
//! 4. A node with a record to send drives IRQ until a transfer has carried it. IRQ is shared by
//!    a bus's nodes, so the master polls each node in turn while it is asserted.
//!
//! A master that clocks a node before it has re-armed reads bytes that fail the magic. The
//! master leaves a gap between transfers to one node, and retries after it.
//!
//! **The P4 checks only this framing**, then forwards the records verbatim. It needs no
//! knowledge of what a node sends, so a change to a node's messages costs no P4 reflash.

use crate::link::{MAX_NODE_PAYLOAD, crc16};

mod record;

pub use record::{Packer, RECORD_HEADER_LEN, RECORD_MAX, Record, RecordType, Records, records};

/// Bytes in every transfer, each way.
pub const TRANSFER_LEN: usize = 1040;
/// The first two bytes of every transfer: "WB" in little-endian order.
pub const MAGIC: u16 = 0x4257;
/// This framing's version. A transfer naming another is not this build's to read.
pub const BUS_VERSION: u8 = 1;
/// Bytes before the payload.
pub const HEADER_LEN: usize = 10;
/// Where the CRC sits. It covers every byte before it.
pub const CRC_OFFSET: usize = TRANSFER_LEN - 2;
/// The most payload one transfer carries.
pub const PAYLOAD_MAX: usize = CRC_OFFSET - HEADER_LEN;
/// The flag that says the sender has more queued after this transfer.
pub const MORE: u8 = 0x01;

const _: () = assert!(TRANSFER_LEN.is_multiple_of(4), "DMA wants a multiple of 4");
const _: () = assert!(RECORD_MAX >= MAX_NODE_PAYLOAD, "one transfer carries any node frame");

/// The fields a sender stamps on a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// [`MORE`], or nothing.
    pub flags: u8,
    /// The sender's counter.
    pub seq: u16,
    /// The last seq the sender received under a good CRC.
    pub ack: u16,
}

impl Header {
    /// Whether the sender has more queued after this transfer.
    #[must_use]
    pub const fn more(&self) -> bool {
        self.flags & MORE != 0
    }
}

/// A transfer that passed [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer<'a> {
    /// What the sender stamped.
    pub header: Header,
    /// The payload's `len` bytes.
    pub payload: &'a [u8],
}

impl<'a> Transfer<'a> {
    /// The records the payload carries.
    #[must_use]
    pub fn records(&self) -> Records<'a> {
        records(self.payload)
    }
}

/// Why [`verify`] refused a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The first two bytes are not [`MAGIC`]: no transfer at all, such as an empty slot or a
    /// slave that was not armed.
    BadMagic,
    /// The CRC does not match.
    BadCrc,
    /// A good transfer naming another [`BUS_VERSION`].
    OtherVersion(u8),
    /// A `len` past [`PAYLOAD_MAX`] under a good CRC.
    BadLen,
}

/// Writes the header, zeroes the payload past `len` and writes the CRC. The caller has
/// already written `len` payload bytes at [`HEADER_LEN`].
///
/// # Panics
/// If `len` is past [`PAYLOAD_MAX`].
pub fn seal(buf: &mut [u8; TRANSFER_LEN], header: Header, len: usize) {
    assert!(len <= PAYLOAD_MAX, "payload of {len} bytes past PAYLOAD_MAX");
    buf[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    buf[2] = BUS_VERSION;
    buf[3] = header.flags;
    buf[4..6].copy_from_slice(&header.seq.to_le_bytes());
    buf[6..8].copy_from_slice(&header.ack.to_le_bytes());
    // `len` fits: PAYLOAD_MAX is below u16::MAX.
    buf[8..10].copy_from_slice(&(len as u16).to_le_bytes());
    buf[HEADER_LEN + len..CRC_OFFSET].fill(0);
    let crc = crc16(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
}

/// Checks magic, then CRC, then version, then `len`.
///
/// The CRC precedes the version so a corrupted byte 2 reads as corruption, not as another
/// version.
///
/// # Errors
/// The first check that failed.
pub fn verify(buf: &[u8; TRANSFER_LEN]) -> Result<Transfer<'_>, VerifyError> {
    if u16::from_le_bytes([buf[0], buf[1]]) != MAGIC {
        return Err(VerifyError::BadMagic);
    }
    if crc16(&buf[..CRC_OFFSET]) != u16::from_le_bytes([buf[CRC_OFFSET], buf[CRC_OFFSET + 1]]) {
        return Err(VerifyError::BadCrc);
    }
    if buf[2] != BUS_VERSION {
        return Err(VerifyError::OtherVersion(buf[2]));
    }
    let len = usize::from(u16::from_le_bytes([buf[8], buf[9]]));
    if len > PAYLOAD_MAX {
        return Err(VerifyError::BadLen);
    }
    Ok(Transfer {
        header: Header {
            flags: buf[3],
            seq: u16::from_le_bytes([buf[4], buf[5]]),
            ack: u16::from_le_bytes([buf[6], buf[7]]),
        },
        payload: &buf[HEADER_LEN..HEADER_LEN + len],
    })
}

/// How a received seq follows the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqNote {
    /// One past the last.
    InOrder,
    /// The first seq seen.
    First,
    /// Anything else: transfers were lost, or the sender restarted.
    Gap,
}

/// Follows a peer's seq across good transfers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeqCheck {
    last: Option<u16>,
}

impl SeqCheck {
    /// A check that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// The last seq noted, the ack to send back, or 0 before the first.
    #[must_use]
    pub fn ack(&self) -> u16 {
        self.last.unwrap_or(0)
    }

    /// Notes `seq` and says how it follows the last. After a gap, `seq` is the new last.
    pub fn note(&mut self, seq: u16) -> SeqNote {
        let note = match self.last {
            None => SeqNote::First,
            Some(last) if seq == last.wrapping_add(1) => SeqNote::InOrder,
            Some(_) => SeqNote::Gap,
        };
        self.last = Some(seq);
        note
    }
}
