//! The UART link between an ESP32-P4 bridge and the board's ESP32-C6.
//!
//! UART1 on both chips, over the board's SDIO traces: [`BAUD_RATE`], 8N1, hardware RTS/CTS.
//! Nothing is clocked, so each side writes only when it has something to say, and each
//! direction runs on its own.
//!
//! A packet on the wire is the COBS encoding of a body, then `0x00`. The body, little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 2 | [`MAGIC`] |
//! | 2 | 1 | [`CORE_VERSION`] |
//! | 3 | 1 | [`Kind`] |
//! | 4 | 4 | seq: the sender's own counter, +1 per packet |
//! | 8 | 2 | len: payload bytes, at most [`PAYLOAD_MAX`] |
//! | 10 | 2 | reserved, zero |
//! | 12 | len | payload |
//! | 12 + len | 2 | CRC-16/CCITT-FALSE ([`crc16`]) over every byte before it |
//!
//! A byte stream with no clock needs resync and has no reason to pad. The next `0x00` realigns
//! [`PacketReader`], so garbage at boot or a line error costs one packet. A COBS failure or an
//! oversize packet reads as [`VerifyError::BadCrc`], since all three are corruption.
//!
//! The C6 sends a hello every [`HELLO_EVERY_MS`] until it hears the P4's. The P4 answers each
//! hello with one of its own, and sends one for no other reason.
//!
//! **Frozen forever, even before 1.0: UART1 on the pins each firmware names, [`BAUD_RATE`] and
//! the line settings, the COBS packet and body above with [`MAGIC`], [`CORE_VERSION`] and the
//! CRC, the [`Kind`] codes, the hello exchange, [`Hello`]'s layout with its image digest, and
//! the [`update`] payload.** A P4 build must always be able to identify and update any C6
//! build, so a link change never sends the user back to a wired flash. Everything else, the
//! [`Up`] and [`Down`] messages a [`Kind::Messages`] packet carries, changes freely between
//! builds and is named by [`LINK_REVISION`]. A P4 whose hello exchange finds another revision
//! does not bridge.
//!
//! The P4 image carries the C6's. At boot the P4 compares the digest in the C6's hello with
//! the bundled image's, and sends the bundle over [`Kind::Update`] when they differ
//! ([`crate::split`]). A C6 whose hello carries no digest cannot take the bundle and needs one
//! wired flash.
//!
//! [`PAYLOAD_MAX`] is 2034, frozen with the rest, so one packet carries any [`update`] op.

use crate::link::{CobsStream, FrameAccumulator, crc16};

mod hello;
mod message;
pub mod update;

pub use hello::{HELLO_IMAGE_LEN, HELLO_LEN, Hello};
pub use message::{
    Down, Held, LINK_REVISION, Packer, RECORD_MAX, RadioStatus, Up, decode_down, decode_up,
    encode_down, encode_up, records,
};

/// The link's baud rate. Both ends take it exactly or not at all.
pub const BAUD_RATE: u32 = 4_000_000;
/// RX FIFO bytes at which the UART hands them on. Below [`RTS_THRESHOLD`], so a read normally
/// keeps the FIFO short of holding the sender back.
pub const FIFO_FULL_THRESHOLD: u16 = 64;
/// RX FIFO bytes at which RTS tells the sender to hold. The FIFO holds 128, which leaves room
/// for what the sender has already started.
pub const RTS_THRESHOLD: u8 = 96;
/// Bytes in each receive DMA buffer. A read ends at this many bytes or when the line goes idle,
/// and the hardware's limit is 4095. A multiple of the P4's 64-byte cache line.
pub const RX_BUF_LEN: usize = 4032;
/// How often the C6 sends its hello until it hears the P4's.
pub const HELLO_EVERY_MS: u64 = 100;
/// How often a P4 that is updating the C6 asks its [`update::Sender`] whether it is due to send,
/// with no status to prompt it.
pub const SENDER_POLL_MS: u64 = 50;

/// The first two bytes of every body: "WT" in little-endian order.
pub const MAGIC: u16 = 0x5754;
/// The frozen core's version. It never changes; a C6 naming another is not ours to read.
pub const CORE_VERSION: u8 = 1;
/// Bytes before the payload.
pub const HEADER_LEN: usize = 12;
/// Bytes of CRC after the payload.
pub const CRC_LEN: usize = 2;
/// The most payload one packet carries.
pub const PAYLOAD_MAX: usize = 2034;
/// The longest body: a full payload between its header and its CRC.
pub const BODY_MAX: usize = HEADER_LEN + PAYLOAD_MAX + CRC_LEN;
/// The most bytes one packet takes on the wire: the longest body, one COBS code byte per 254
/// bytes of it, and the terminator.
pub const WIRE_MAX: usize = BODY_MAX + BODY_MAX.div_ceil(254) + 1;

/// What a packet's payload holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
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
            1 => Some(Self::Hello),
            2 => Some(Self::Messages),
            3 => Some(Self::Update),
            _ => None,
        }
    }
}

/// A packet that passed [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// What the payload holds.
    pub kind: Kind,
    /// The sender's counter.
    pub seq: u32,
    /// The payload's `len` bytes.
    pub payload: &'a [u8],
}

/// Why a packet was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The first two bytes are not [`MAGIC`]: no packet at all, such as a boot's line noise.
    BadMagic,
    /// The CRC does not match, the body is too short to hold one, or the bytes were not a
    /// COBS packet of a size this link sends.
    BadCrc,
    /// A good packet naming another [`CORE_VERSION`].
    OtherCore(u8),
    /// A `len` past [`PAYLOAD_MAX`], or other than the payload's, under a good CRC.
    BadLen,
    /// A kind this build does not know.
    UnknownKind(u8),
}

/// Writes the header and the CRC around `len` payload bytes the caller has already written at
/// [`HEADER_LEN`], and returns the body's length.
///
/// # Panics
/// If `len` is past [`PAYLOAD_MAX`], or `body` is too short to hold it.
pub fn seal(body: &mut [u8], kind: Kind, seq: u32, len: usize) -> usize {
    assert!(len <= PAYLOAD_MAX, "payload of {len} bytes past PAYLOAD_MAX");
    let crc_at = HEADER_LEN + len;
    body[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    body[2] = CORE_VERSION;
    body[3] = kind as u8;
    body[4..8].copy_from_slice(&seq.to_le_bytes());
    body[8..10].copy_from_slice(&(len as u16).to_le_bytes());
    body[10..12].fill(0);
    let crc = crc16(&body[..crc_at]);
    body[crc_at..crc_at + CRC_LEN].copy_from_slice(&crc.to_le_bytes());
    crc_at + CRC_LEN
}

/// Checks magic, then CRC, then core version, then `len`, then kind. `body` is one whole
/// body, with its CRC as its last two bytes.
///
/// The CRC precedes the version so a corrupted byte 2 reads as corruption, not as another
/// core.
///
/// # Errors
/// The first check that failed.
pub fn verify(body: &[u8]) -> Result<Packet<'_>, VerifyError> {
    if body.len() < HEADER_LEN + CRC_LEN {
        return Err(VerifyError::BadCrc);
    }
    if u16::from_le_bytes([body[0], body[1]]) != MAGIC {
        return Err(VerifyError::BadMagic);
    }
    let crc_at = body.len() - CRC_LEN;
    if crc16(&body[..crc_at]) != u16::from_le_bytes([body[crc_at], body[crc_at + 1]]) {
        return Err(VerifyError::BadCrc);
    }
    if body[2] != CORE_VERSION {
        return Err(VerifyError::OtherCore(body[2]));
    }
    let len = usize::from(u16::from_le_bytes([body[8], body[9]]));
    if len > PAYLOAD_MAX || HEADER_LEN + len != crc_at {
        return Err(VerifyError::BadLen);
    }
    let kind = Kind::from_u8(body[3]).ok_or(VerifyError::UnknownKind(body[3]))?;
    Ok(Packet {
        kind,
        seq: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        payload: &body[HEADER_LEN..crc_at],
    })
}

/// Writes `body` to `wire` as a whole packet, terminator included, and returns its length.
///
/// # Panics
/// If `wire` is too short for it. [`WIRE_MAX`] holds any body of up to [`BODY_MAX`].
pub fn encode_packet(body: &[u8], wire: &mut [u8]) -> usize {
    let mut stream = CobsStream::new();
    let len = stream.fill(body, wire);
    assert!(stream.is_done(), "a {}-byte body does not fit {} bytes", body.len(), wire.len());
    len
}

/// Reassembles packets from what the UART hands over, however it splits them.
///
/// Const to build, so firmware keeps it in a static.
#[derive(Debug)]
pub struct PacketReader {
    // Without the terminator, which ends a frame and is never stored.
    frames: FrameAccumulator<{ WIRE_MAX - 1 }>,
}

impl PacketReader {
    /// A reader at the start of a packet.
    #[must_use]
    pub const fn new() -> Self {
        Self { frames: FrameAccumulator::new() }
    }

    /// Feeds one byte. Returns what became of a packet when this byte ended one.
    pub fn push(&mut self, byte: u8) -> Option<Result<Packet<'_>, VerifyError>> {
        let oversize = byte == 0 && self.frames.overflowed();
        match self.frames.push(byte) {
            Some(frame) => Some(decode(frame)),
            None if oversize => Some(Err(VerifyError::BadCrc)),
            None => None,
        }
    }
}

impl Default for PacketReader {
    fn default() -> Self {
        Self::new()
    }
}

fn decode(frame: &mut [u8]) -> Result<Packet<'_>, VerifyError> {
    let len = cobs::decode_in_place(frame).map_err(|_| VerifyError::BadCrc)?;
    verify(&frame[..len])
}

/// How a received seq relates to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqNote {
    /// One past the last.
    InOrder,
    /// The first seq seen.
    First,
    /// Anything else: packets were lost, or the sender restarted. A sender never sends a
    /// packet twice, so an equal seq is a restart too.
    Gap,
}

/// Follows the other side's seq across packets.
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
            Some(_) => SeqNote::Gap,
        };
        self.last = Some(seq);
        note
    }
}
