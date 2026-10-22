//! The SPI link between an ESP32-P4 bridge and the ESP32-C6 that runs its radio.
//!
//! The P4 is master and clocks every transfer. Each one is exactly [`TRANSFER_LEN`] bytes
//! each way, little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 2 | [`MAGIC`] |
//! | 2 | 1 | [`CORE_VERSION`] |
//! | 3 | 1 | [`Kind`] |
//! | 4 | 4 | seq: the sender's own counter, +1 per transfer |
//! | 8 | 2 | len: payload bytes, at most [`PAYLOAD_MAX`] |
//! | 10 | 2 | reserved, zero |
//! | 12 | len | payload; bytes past `len` are zero |
//! | [`CRC_OFFSET`] | 2 | CRC-16/CCITT-FALSE ([`crc16`]) over bytes `0..CRC_OFFSET` |
//!
//! Every transfer follows the READY handshake:
//!
//! 1. The C6 drives READY high once it has armed a transfer.
//! 2. The P4 waits for READY's rising edge.
//! 3. The P4 clocks exactly [`TRANSFER_LEN`] bytes with CS low.
//! 4. The C6 drops READY once the transfer completes.
//!
//! **Frozen forever, even before 1.0: the READY handshake, [`TRANSFER_LEN`], the header
//! above with [`MAGIC`], [`CORE_VERSION`] and the CRC, the [`Kind`] codes, [`Hello`]'s
//! layout with its image digest, and the [`update`] payload.** A P4 build must always be
//! able to identify and update any C6 build, so a link change never sends the user back to a
//! wired flash. Everything else, the [`Up`] and [`Down`] messages a [`Kind::Messages`]
//! transfer carries, changes freely between builds and is named by [`LINK_REVISION`]. A P4
//! whose hello exchange finds another revision does not bridge.
//!
//! The P4 image carries the C6's. At boot the P4 compares the digest in the C6's hello with
//! the bundled image's, and sends the bundle over [`Kind::Update`] when they differ
//! ([`crate::split`]). A C6 whose hello carries no digest predates the update path and needs
//! one wired flash.
//!
//! [`TRANSFER_LEN`] is 2048 so one transfer carries the largest [`Down`], a 1,470-byte
//! [`HostToBridge::SendEspNow`](crate::link::HostToBridge::SendEspNow), without
//! fragmentation.

use crate::link::crc16;

mod hello;
mod message;
pub mod update;

pub use hello::{HELLO_IMAGE_LEN, HELLO_LEN, Hello};
pub use message::{
    Down, Held, LINK_REVISION, Packer, RECORD_MAX, RadioStatus, Up, decode_down, decode_up,
    encode_down, encode_up, records,
};

/// Bytes in every transfer, each way.
pub const TRANSFER_LEN: usize = 2048;
/// The first two bytes of every transfer: "WT" in little-endian order.
pub const MAGIC: u16 = 0x5754;
/// The frozen core's version. It never changes; a C6 naming another is not ours to read.
pub const CORE_VERSION: u8 = 1;
/// Bytes before the payload.
pub const HEADER_LEN: usize = 12;
/// Where the CRC sits. It covers every byte before it.
pub const CRC_OFFSET: usize = TRANSFER_LEN - 2;
/// The most payload one transfer carries.
pub const PAYLOAD_MAX: usize = CRC_OFFSET - HEADER_LEN;

/// What a transfer's payload holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Nothing: the sender had nothing to say, and the transfer only carries its seq.
    Idle = 0,
    /// A [`Hello`].
    Hello = 1,
    /// Records of [`Up`] or [`Down`] messages; see [`Packer`].
    Messages = 2,
    /// An [`update::Op`]: the P4 sending the C6 an image, or the C6's status.
    Update = 3,
}

impl Kind {
    /// The kind a header's byte names, if this build knows it.
    #[must_use]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Idle),
            1 => Some(Self::Hello),
            2 => Some(Self::Messages),
            3 => Some(Self::Update),
            _ => None,
        }
    }
}

/// A transfer that passed [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer<'a> {
    /// What the payload holds.
    pub kind: Kind,
    /// The sender's counter.
    pub seq: u32,
    /// The payload's `len` bytes.
    pub payload: &'a [u8],
}

/// Why [`verify`] refused a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The first two bytes are not [`MAGIC`]: no transfer at all, such as a slave that was
    /// not armed.
    BadMagic,
    /// The CRC does not match.
    BadCrc,
    /// A good transfer naming another [`CORE_VERSION`].
    OtherCore(u8),
    /// A `len` past [`PAYLOAD_MAX`] under a good CRC.
    BadLen,
    /// A kind this build does not know.
    UnknownKind(u8),
}

/// Writes the header, zeroes the payload past `len` and writes the CRC. The caller has
/// already written `len` payload bytes at [`HEADER_LEN`].
///
/// # Panics
/// If `len` is past [`PAYLOAD_MAX`].
pub fn seal(buf: &mut [u8; TRANSFER_LEN], kind: Kind, seq: u32, len: usize) {
    assert!(len <= PAYLOAD_MAX, "payload of {len} bytes past PAYLOAD_MAX");
    buf[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    buf[2] = CORE_VERSION;
    buf[3] = kind as u8;
    buf[4..8].copy_from_slice(&seq.to_le_bytes());
    buf[8..10].copy_from_slice(&(len as u16).to_le_bytes());
    buf[10..12].fill(0);
    buf[HEADER_LEN + len..CRC_OFFSET].fill(0);
    let crc = crc16(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
}

/// Checks magic, then CRC, then core version, then `len`, then kind.
///
/// The CRC precedes the version so a corrupted byte 2 reads as corruption, not as another
/// core.
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
    if buf[2] != CORE_VERSION {
        return Err(VerifyError::OtherCore(buf[2]));
    }
    let len = usize::from(u16::from_le_bytes([buf[8], buf[9]]));
    if len > PAYLOAD_MAX {
        return Err(VerifyError::BadLen);
    }
    let kind = Kind::from_u8(buf[3]).ok_or(VerifyError::UnknownKind(buf[3]))?;
    Ok(Transfer {
        kind,
        seq: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        payload: &buf[HEADER_LEN..HEADER_LEN + len],
    })
}

/// How a received seq relates to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqNote {
    /// One past the last.
    InOrder,
    /// The first seq seen.
    First,
    /// The same as the last: the sender did not see this transfer land and sent it again.
    Repeat,
    /// Anything else: transfers were lost, or the sender restarted.
    Gap,
}

/// Follows the other side's seq across transfers.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeqCheck {
    last: Option<u32>,
}

impl SeqCheck {
    /// A check that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Notes `seq` and says how it follows the last. After a gap, `seq` is the new last.
    pub fn note(&mut self, seq: u32) -> SeqNote {
        let note = match self.last {
            None => SeqNote::First,
            Some(last) if seq == last.wrapping_add(1) => SeqNote::InOrder,
            Some(last) if seq == last => SeqNote::Repeat,
            Some(_) => SeqNote::Gap,
        };
        self.last = Some(seq);
        note
    }
}
