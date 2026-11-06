//! The node bus: SPI between a P4 bridge, as controller, and its nodes, as slaves. The framing,
//! the acknowledgment that makes it reliable, and the messages its records carry.
//!
//! The P4 clocks every transfer. Each one is exactly [`TRANSFER_LEN`] bytes each way,
//! little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 2 | [`MAGIC`] |
//! | 2 | 1 | [`BUS_VERSION`] |
//! | 3 | 1 | flags: bit 0 [`ACKS`], `ack` is valid; other bits zero |
//! | 4 | 2 | seq: the sender's counter, +1 per transfer it arms, from 0 at boot |
//! | 6 | 2 | ack: with [`ACKS`], the seq of the last good transfer from the peer that carried records |
//! | 8 | 2 | len: payload bytes, at most [`PAYLOAD_MAX`] |
//! | 10 | len | payload: records ([`Packer`], [`records`]); bytes past `len` are zero |
//! | [`CRC_OFFSET`] | 2 | CRC-16/CCITT-FALSE ([`crc16`]) over bytes `0..CRC_OFFSET` |
//!
//! **The length is fixed** because a slave is armed for one length before the controller clocks,
//! and cannot learn a length from the transfer it is in. A fixed full-duplex transfer is the
//! path the P4–C6 bench proved error-free (`docs/p4-spi-link-findings.md`). [`TRANSFER_LEN`]
//! carries one record holding the longest body either way, an [`UpdateChunk`] down, and is a
//! multiple of 4 for the DMA.
//!
//! **A node re-arms after every transfer, and asks for one through IRQ.**
//!
//! 1. The node arms its slave with the next transfer before the controller clocks.
//! 2. The controller clocks [`TRANSFER_LEN`] bytes with the node's CS low.
//! 3. CS rises, and the node arms the next transfer at once.
//! 4. A node with a record to send drives IRQ until a transfer has carried it. IRQ is shared by
//!    a bus's nodes, so the controller polls each node in turn while it is asserted.
//!
//! A controller that clocks a node before it has re-armed reads bytes that fail the magic. The
//! controller leaves a gap between transfers to one node, and retries after it.
//!
//! **A transfer that carries records is acknowledged; an idle one needs nothing.** Each end
//! stamps `ack` with the seq of the last good record-carrying transfer it took from the peer
//! ([`AckTracker`]), and sets [`ACKS`] once there is one, so seq 0 needs no special case. Idle
//! transfers never move it, so an idle transfer that arrives in place of a lost data transfer
//! cannot acknowledge it. Each direction of each slot is stop-and-wait ([`AckWait`]): a sender
//! puts records in a transfer, then sends only idle transfers until the peer's `ack` names that
//! transfer, or [`ACK_TIMEOUT_MS`] passes. It then sends the same records in a new transfer, as a
//! MAC layer retries, and gives up after [`SEND_TRIES`]. So a resent record may arrive twice;
//! every message here is safe to take twice. The P4 keeps a queue per slot ([`Downlink`]) and
//! packs as many queued records as fit into each transfer.
//!
//! A sender that sends no records for 65,536 transfers and then sends at exactly the seq its
//! peer last acknowledged reads a stale ack as its own.
//!
//! **Frozen forever, even before 1.0:**
//!
//! - the transfer: SPI mode 0 on the node's D8 SCLK, D10 MOSI, D9 MISO, D7 CS and D0 IRQ,
//!   [`TRANSFER_LEN`], the header above with [`MAGIC`], [`BUS_VERSION`] and [`ACKS`],
//!   [`HEADER_LEN`], [`PAYLOAD_MAX`], [`CRC_OFFSET`] and the CRC, with zeros past `len`;
//! - the handshake: the node re-arms after every transfer, and raises IRQ to ask for one;
//! - the ack rule: `ack` names the last good record-carrying transfer, idle transfers never move
//!   it, and each direction is stop-and-wait;
//! - the record layout ([`RECORD_HEADER_LEN`], [`records`]), and a receiver skipping a type it
//!   does not know;
//! - the heartbeat's identity: up type 1 opens with the prefix and the node's MAC
//!   ([`HEARTBEAT_IDENTITY_LEN`], [`Heartbeat::identity`]), read from a body of at least those
//!   11 bytes, at any length past them, with the [`Board`] codes, [`CAP_FLAG_5G`] and
//!   [`CAP_FLAG_TRIAL`];
//! - the update conversation: up type 3 and down types 3 to 7, their layouts, the
//!   [`UpdateState`] codes, [`UPDATE_CHUNK`], and the receiver's side that [`crate::ota`]
//!   describes;
//! - confirming a trial: an [`UpdateConfirm`], or any update record, confirms an image on trial;
//! - the `trial` sector's layout ([`crate::ota::trial`]), which the image that installs an update
//!   writes and the image it installs reads;
//! - the node's partition table: an update writes only an app slot, so every later image runs on
//!   the table of the node's last wired flash (`firmware/node/partitions.csv`). Each later image
//!   fits the 0x1E0000 app slots, `otadata` stays where it is, and the image finds its update
//!   slot and `trial` sector by the rule of `locate` in `firmware/common/src/ota.rs`: the first
//!   app slot it did not boot from, factory and test excluded, and the first `data`/`undefined`
//!   partition, which must be at least one sector;
//! - what any P4 owes a node of any build, whose re-arm time, ack timeout and tries its image
//!   fixes: at least 1 ms from the end of one transfer to a node to the start of the next, so it
//!   has re-armed; and a transfer to a node whose IRQ is asserted, or whose records the P4 has
//!   yet to acknowledge, well within the node's [`ACK_TIMEOUT_MS`]. Otherwise the node gives up
//!   on records never clocked out, and resends unacknowledged ones until [`SEND_TRIES`] run out.
//!   Every node built so far carries 1 s, so every later P4 answers within 1 s.
//!
//! A node may be reachable only through the bus, so any later host and P4 must be able to
//! identify and update any node build, and a bus change never sends the user back to a wired
//! flash. Everything else changes freely between builds and is named by the node's version in
//! the heartbeat prefix ([`NODE_VERSION`]): the clock rate, within what the node's SPI slave
//! takes, which is the chip's limit rather than the image's; [`ACK_TIMEOUT_MS`] and
//! [`SEND_TRIES`], as each sender's own policy; the P4's pins, and its scheduling beyond what it
//! owes a node; up type 2, down types 1 and 2, and the heartbeat past its MAC. A core type code
//! is never reused, and a new message takes a new code.
//!
//! **The P4 checks only this framing**, then forwards the records verbatim. A record's type
//! byte is the message type ([`UpType`], [`DownType`]) and its body the message body; the
//! messages are in [`UpMsg`] and [`DownMsg`]. The P4 needs no knowledge of them, so a change to
//! a node's messages costs no P4 reflash.
//!
//! [`crc16`]: crate::link::crc16

use crate::link::{MAX_NODE_DOWN, MAX_NODE_PAYLOAD, crc16};

mod ack;
mod assign;
mod downlink;
mod health;
mod heartbeat;
mod msg;
mod node_board;
mod record;
mod security;
mod sighting;
mod slot;
mod update;
mod version;

pub use ack::{ACK_TIMEOUT_MS, AckPoll, AckTracker, AckWait, SEND_TRIES};
pub use assign::{ASSIGN_FLAG_BLE, ASSIGN_LEN, Assign, CLEAR_LEN, Clear, wire_epoch};
pub use downlink::{Downlink, FLIGHT_MAX, Ids};
pub use health::{DieTemp, Vdd};
pub use heartbeat::{
    Board, CAP_FLAG_5G, CAP_FLAG_TRIAL, Capabilities, HEARTBEAT_IDENTITY_LEN, HEARTBEAT_LEN,
    HEARTBEAT_PREFIX_LEN, Heartbeat,
};
pub use msg::{DecodeError, DownMsg, DownType, UpMsg, UpType};
pub use node_board::{NodeBoard, NodeBoardError, SlotLabel};
pub use record::{Packer, RECORD_HEADER_LEN, RECORD_MAX, Record, Records, records};
pub use security::Security;
pub use sighting::{
    EXT_MAX, RecordKind, SIGHTING_BATCH_HEADER, SIGHTING_BATCH_MAX, SIGHTING_RECORD_MAX,
    SIGHTING_RECORD_MIN, SIGHTINGS_PER_BATCH_MAX, SSID_MAX, SightingBatch, SightingBatchWriter,
    SightingMsg,
};
pub use slot::{BUSES, MAX_SLOTS, PER_BUS, Slot, SlotError};
pub use update::{
    UPDATE_ABORT_LEN, UPDATE_BEGIN_LEN, UPDATE_CHUNK, UPDATE_CHUNK_HEADER, UPDATE_CHUNK_MAX,
    UPDATE_COMMIT_LEN, UPDATE_CONFIRM_LEN, UPDATE_STATUS_LEN, UpdateAbort, UpdateBegin,
    UpdateChunk, UpdateCommit, UpdateConfirm, UpdateState, UpdateStatus,
};
pub use version::{Compat, NODE_VERSION, Version};

/// Bytes in every transfer, each way.
pub const TRANSFER_LEN: usize = 1044;
/// The first two bytes of every transfer: "WB" in little-endian order.
pub const MAGIC: u16 = 0x4257;
/// The frozen core's version. It never changes; a transfer naming another is not this build's
/// to read.
pub const BUS_VERSION: u8 = 1;
/// Bytes before the payload.
pub const HEADER_LEN: usize = 10;
/// Where the CRC sits. It covers every byte before it.
pub const CRC_OFFSET: usize = TRANSFER_LEN - 2;
/// The most payload one transfer carries.
pub const PAYLOAD_MAX: usize = CRC_OFFSET - HEADER_LEN;
/// The flag that says `ack` names a transfer.
pub const ACKS: u8 = 0x01;

const _: () = assert!(TRANSFER_LEN.is_multiple_of(4), "DMA wants a multiple of 4");
const _: () = assert!(RECORD_MAX >= MAX_NODE_PAYLOAD, "one transfer carries any up body");
const _: () = assert!(RECORD_MAX >= MAX_NODE_DOWN, "one transfer carries any down body");

/// The fields a sender stamps on a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// [`ACKS`], or nothing.
    pub flags: u8,
    /// The sender's counter.
    pub seq: u16,
    /// With [`ACKS`], the last record-carrying seq the sender took from its peer.
    pub ack: u16,
}

impl Header {
    /// A header for transfer `seq`, acknowledging `ack` when there is one.
    #[must_use]
    pub const fn new(seq: u16, ack: Option<u16>) -> Self {
        match ack {
            Some(ack) => Self { flags: ACKS, seq, ack },
            None => Self { flags: 0, seq, ack: 0 },
        }
    }

    /// The transfer this one acknowledges, if any.
    #[must_use]
    pub const fn acked(&self) -> Option<u16> {
        if self.flags & ACKS != 0 { Some(self.ack) } else { None }
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

    /// Whether the transfer carries records, and so is to be acknowledged.
    #[must_use]
    pub const fn carries_records(&self) -> bool {
        !self.payload.is_empty()
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
