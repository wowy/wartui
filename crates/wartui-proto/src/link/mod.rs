//! The link between the host and the P4 bridge: USB to the board's CH343, then UART0 at
//! [`HOST_BAUD`] to the P4.
//!
//! The link has a version byte, a checksum, and framing that resynchronizes from arbitrary
//! junk.
//!
//! ```text
//! COBS( version:u8 || postcard(message) || crc16:u16le ) || 0x00
//! ```
//!
//! The version byte sits *outside* the postcard blob, so a mismatch is detectable without
//! a deserialize that cannot be trusted. The CRC rejects what the UART leg corrupts, which
//! nothing retries, half-written frames, and the bootloader's log a reset prints down the same
//! pipe.
//!
//! The host speaks to a node by its node-bus [`Slot`]: a [`HostToBridge::ToNode`] carries one
//! record for it, of up to [`MAX_NODE_DOWN`] bytes, and the P4 answers each with
//! [`BridgeToHost::Delivered`] or [`BridgeToHost::Undelivered`]. Every record a node sends
//! arrives as a [`BridgeToHost::FromNode`] of up to [`MAX_NODE_PAYLOAD`] bytes. The P4 reads
//! neither; [`crate::bus`] has the messages.

use crate::bus::{NodeBoard, Slot};
use crate::mac::Mac;
use crate::ota::image::DIGEST_LEN;
use crate::radio_link::update::OP_MAX;
use crate::reset::ResetCause;
use heapless::{String, Vec};
use serde::Serialize;

mod bytes;
mod frame;

pub(crate) use frame::CobsStream;
pub use frame::{
    FrameAccumulator, LinkError, crc16, decode_frame, encode_frame, encode_host_frame,
};

/// The host link's baud rate, 8N1, set at both ends.
///
/// The fastest rate the CH343 carries cleanly, and one the P4 divides exactly from its 40 MHz
/// crystal; `docs/p4-host-uart-findings.md` has the sweep.
pub const HOST_BAUD: u32 = 4_000_000;

/// The revision of this protocol both ends must agree on.
///
/// Held at 1 until 1.0, whatever the message enums do: both ends are flashed from one tree,
/// so there is no older peer for it to protect.
///
/// The cost is that a host and a bridge built from different trees meet as an
/// undecodable frame, not a named mismatch. Postcard writes an enum variant as its index
/// and a struct's fields in order. A new variant is an index the old build has no case
/// for. A new field on [`BridgeToHost::Ready`] shifts everything after it, so the bridge
/// reads as one that answered nothing. Every addition to these enums goes on the end.
pub const LINK_PROTO_VERSION: u8 = 1;

/// The longest body a node sends. A [`crate::bus::SightingBatch`] fills it exactly. The P4
/// drops a longer record, and counts it in [`BridgeToHost::Status`]'s `rx_oversize`.
pub const MAX_NODE_PAYLOAD: usize = 1024;

/// The longest body the host sends a node: a full [`crate::bus::UpdateChunk`].
pub const MAX_NODE_DOWN: usize = 1029;

/// Buffer size for one encoded bridge-to-host frame: a [`BridgeToHost::FromNode`] carrying a
/// full [`MAX_NODE_PAYLOAD`], with room for framing.
pub const MAX_FRAME: usize = 1152;

/// Buffer size for one encoded host-to-bridge frame: a [`HostToBridge::ToNode`] carrying a
/// full [`MAX_NODE_DOWN`], or an update op carrying a full chunk, with room for framing.
pub const MAX_HOST_FRAME: usize = 1152;

/// A record body a node sent, in transit to the host.
pub type FromNodeBody = Vec<u8, MAX_NODE_PAYLOAD>;

/// A record body for a node, in transit to the P4.
pub type ToNodeBody = Vec<u8, MAX_NODE_DOWN>;

/// Short identifier, such as a firmware version.
pub type ShortStr = String<32>;

/// A log line from the bridge.
pub type LogStr = String<96>;

/// The most receiver bytes one [`BridgeToHost::Gps`] carries. The bridge batches several
/// I2C reads into one frame, so a frame per read cannot push sightings out of its bulk
/// ring (`firmware/common/src/gps.rs`). An epoch's GGA and RMC fit with room to spare.
pub const GPS_FRAME: usize = 512;

/// Bytes read from the GPS receiver, in transit to the host.
pub type GpsBytes = Vec<u8, GPS_FRAME>;

/// The most bytes one [`HostToBridge::GpsWrite`] carries. A receiver configuration
/// message the host composes fits with room to spare.
pub const GPS_WRITE_MAX: usize = 256;

/// Bytes the host composed for the GPS receiver, in transit to the bridge.
pub type GpsWriteBytes = Vec<u8, GPS_WRITE_MAX>;

/// One encoded [`crate::radio_link::update::Op`] for a P4 bridge's own image, in transit to it.
pub type UpdateOp = Vec<u8, OP_MAX>;

/// The most bytes one [`BridgeToHost::Update`] carries: an encoded `Op::Status`, with room
/// for its body to grow at the end.
pub const UPDATE_STATUS_MAX: usize = 16;

/// One encoded `Op::Status`, in transit to the host.
pub type UpdateStatusBytes = Vec<u8, UPDATE_STATUS_MAX>;

/// The P4's `Info` [`BridgeToHost::Log`] after a host frame confirms its image on trial.
/// `wartui flash-bridge` waits for it after an update, so both ends read the text from here.
pub const IMAGE_CONFIRMED: &str = "image confirmed by the host";

/// The start of the P4's [`BridgeToHost::Error`] when confirming its image fails; the flash
/// error follows after a colon.
pub const IMAGE_NOT_CONFIRMED: &str = "could not confirm the image";

/// The start of the P4's [`BridgeToHost::Error`] while it updates its C6
/// (`split::Pair::fault`). `wartui flash-bridge` follows the C6's update by it.
pub const RADIO_UPDATING: &str = "updating the radio";

/// Which chip the bridge firmware is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Chip {
    /// Dual-band, same radio as the nodes.
    Esp32C5,
    /// 2.4 GHz only: the radio coprocessor of a P4 bridge.
    Esp32C6,
    /// No radio of its own: it bridges through an ESP32-C6 over UART.
    Esp32P4,
}

/// What a bridge chip's firmware last marked before it reset.
///
/// Carried across a reset in RTC memory and reported in [`BridgeToHost::Ready`],
/// because the interesting resets are the ones nobody was watching. It is only a hint. Each
/// chip's futures share one task and one marker, and the UART link's marks it on every
/// packet, so it says what ran last, not what stopped the chip. A future parked on an
/// `await` leaves the others running, so only a call that never yields holds the marker
/// where it hung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum LoopPhase {
    /// Nothing to report: a power-on, or a reset that did not preserve the marker.
    Unknown,
    /// Still in `main`, before its futures started.
    Boot,
    /// Reading and decoding host commands.
    DrainLink,
    /// Carrying out a host command.
    Command,
    /// Writing queued frames to the host.
    Pump,
    /// Reading or writing a packet over the P4–C6 UART link.
    RadioLink,
}

/// Severity of a [`BridgeToHost::Log`] line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Why the P4 could not deliver a [`HostToBridge::ToNode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Undelivered {
    /// No node answers in that slot.
    NoNode,
    /// The slot's queue had no room.
    QueueFull,
    /// The body is longer than one transfer carries.
    TooLarge,
    /// The node did not acknowledge the transfer that carried it, after every try.
    Timeout,
}

/// Commands the host sends to the bridge.
///
/// The bridge understands framing, and nothing about what a node's bytes mean.
// The payload variants dwarf the rest, but boxing them would need an allocator in the
// bridge firmware, which this crate avoids. These values are built, serialized and
// dropped, never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum HostToBridge {
    /// Ask the bridge to announce itself with [`BridgeToHost::Ready`].
    ///
    /// Sent the moment the host opens the port. The P4 writes nothing until a host frame
    /// arrives (`firmware/p4-bridge/src/host.rs`), so a new connection that did not ask would
    /// wait forever.
    Identify,
    /// Ask for a [`BridgeToHost::Status`].
    GetStatus,
    /// Reboot the bridge.
    Reset,
    /// Bytes to write verbatim to the GPS receiver, composed by the host.
    ///
    /// The bridge does not read them, so a change to the receiver's configuration costs
    /// a `cargo run`, not a reflash. A bridge without a GPS port drops them.
    GpsWrite {
        /// The receiver's own protocol, typically UBX.
        #[serde(with = "bytes")]
        bytes: GpsWriteBytes,
    },
    /// One step of an update to a P4 bridge's own image: a `Begin`, `Chunk`, `Commit` or
    /// `Install` in the [`crate::radio_link::update`] layout, which the P4 answers with a
    /// [`BridgeToHost::Update`]. The P4 handles the same bytes its C6 does, so one codec
    /// serves both links. A bridge that takes no update answers with an
    /// [`BridgeToHost::Error`].
    Update(#[serde(with = "bytes")] UpdateOp),
    /// One record for the node in `slot`, answered by [`BridgeToHost::Delivered`] or
    /// [`BridgeToHost::Undelivered`].
    ToNode {
        /// Where the node sits.
        slot: Slot,
        /// Correlates the answer.
        id: u16,
        /// The record type, a [`crate::bus::DownType`] byte.
        kind: u8,
        /// The record body, already encoded by [`crate::bus`].
        #[serde(with = "bytes")]
        body: ToNodeBody,
    },
    /// Switch the node in `slot` off and on again, through the carrier board, answered by
    /// [`BridgeToHost::PowerCycled`].
    PowerCycle {
        /// Which node.
        slot: Slot,
    },
}

/// Events and replies the bridge sends to the host.
// Unboxed for the reason `HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum BridgeToHost {
    /// Sent in answer to [`HostToBridge::Identify`], when a host's first frame arrives, and
    /// when a new life begins while a host is present (`firmware/p4-bridge/src/host.rs`). The
    /// host checks `proto_version` and refuses a bridge it does not understand.
    Ready {
        /// Which chip this is.
        chip: Chip,
        /// The bridge's identity: its C6's MAC, or the P4's own while no C6 answers.
        mac: Mac,
        /// Bridge firmware version.
        fw_version: ShortStr,
        /// Always [`LINK_PROTO_VERSION`] as the firmware was built with.
        proto_version: u8,
        /// Why this life started. Anything but [`ResetCause::PowerOn`] means
        /// the bridge restarted underneath a host that may not have noticed.
        reset_cause: ResetCause,
        /// Where the previous life stopped, when the reset preserved it.
        last_phase: LoopPhase,
        /// Bytes free in the C6's heap. Nothing wartui writes allocates, so a figure that
        /// falls across a long capture is a leak.
        heap_free: u32,
        /// Milliseconds since this life started, at the moment of announcing.
        ///
        /// Lets the host tell a *new* life from a second answer to an
        /// [`HostToBridge::Identify`] sent twice. A reset of either chip leaves the CH343 on
        /// the bus, so both arrive on one connection, and nothing else in the frame separates
        /// them. See `crates/wartui-bridge/src/serial.rs`.
        uptime_ms: u32,
        /// Whether this board was built with a GPS port.
        ///
        /// A board fact. It does not mean a receiver answered: a missing receiver is
        /// reported as an [`Self::Error`] instead.
        gps: bool,
        /// The ESP32-C6 radio behind the bridge, as the C6 reported it. `None` while the C6
        /// has not answered.
        radio: Option<RadioInfo>,
        /// The digest appended to the booted image ([`crate::ota::image`]), from a bridge
        /// that takes updates over this link. `None` from one that does not. The host reads
        /// it to tell an installed image from a rollback.
        image: Option<[u8; DIGEST_LEN]>,
        /// What carries the nodes, which the P4 reads at boot. It sets the slots driven and the
        /// names an operator reads for them.
        node_board: NodeBoard,
    },
    /// Reply to [`HostToBridge::GetStatus`].
    Status {
        /// Node records forwarded since boot. Oversize records are counted in
        /// `rx_oversize` instead.
        rx_count: u32,
        /// Frames the host queue dropped. See [`crate::host_queue`].
        dropped_tx: u32,
        /// Bridge uptime.
        uptime_ms: u32,
        /// Node records longer than [`MAX_NODE_PAYLOAD`], dropped since boot. No node of
        /// this build sends one.
        rx_oversize: u32,
        /// Fill level of each bounded queue the firmware has. Empty for a bridge with
        /// none to report.
        queues: QueueStats,
        /// The P4–C6 UART link's counters. `None` for a bridge without one.
        radio_link: Option<RadioLinkStats>,
        /// Host-link reads since boot that saw one or more line errors or overflows on the
        /// P4's host UART: FIFO overflows, framing, parity and glitches. One read counts once
        /// however many it saw. An overflow is host bytes lost, as when the P4 holds its
        /// interrupts off for its own flash work.
        host_uart_errors: u32,
    },
    /// Diagnostics, routed through the link rather than printed. `esp-println` would
    /// interleave into the same endpoint and corrupt the framing.
    Log {
        /// Severity.
        level: LogLevel,
        /// Message text.
        message: LogStr,
    },
    /// The bridge could not carry out a command.
    Error {
        /// What went wrong.
        message: LogStr,
    },
    /// Bytes read from the GPS receiver, uninterpreted.
    ///
    /// The receiver's stream verbatim, NMEA and UBX interleaved, split wherever a batch
    /// ended. The host reassembles and parses it.
    Gps {
        /// The bytes, in the order the receiver gave them.
        #[serde(with = "bytes")]
        bytes: GpsBytes,
    },
    /// Where an update to the bridge's own image stands: an `Op::Status` in the
    /// [`crate::radio_link::update`] layout, answering a [`HostToBridge::Update`].
    Update(#[serde(with = "bytes")] UpdateStatusBytes),
    /// A record from the node in `slot`.
    FromNode {
        /// Where the node sits.
        slot: Slot,
        /// The P4's microsecond clock when the transfer that carried it completed. The host
        /// compares it with its own clock to tell how far behind it reads.
        rx_us: u32,
        /// The record type, a [`crate::bus::UpType`] byte.
        kind: u8,
        /// The record body, verbatim.
        #[serde(with = "bytes")]
        body: FromNodeBody,
    },
    /// The node acknowledged the transfer that carried [`HostToBridge::ToNode`] `id`.
    Delivered {
        /// Echoes the request's `id`.
        id: u16,
    },
    /// [`HostToBridge::ToNode`] `id` did not reach its node.
    Undelivered {
        /// Echoes the request's `id`.
        id: u16,
        /// Why.
        reason: Undelivered,
    },
    /// The answer to [`HostToBridge::PowerCycle`]. A request for a slot already waiting or
    /// cycling joins that cycle, which answers once.
    PowerCycled {
        /// Echoes the request's slot.
        slot: Slot,
        /// How it went.
        outcome: PowerOutcome,
    },
}

/// How a [`HostToBridge::PowerCycle`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum PowerOutcome {
    /// The node was switched off, then on again, and is powered now.
    Done,
    /// The bridge has no carrier board, so nothing switches node power.
    NoCarrier,
    /// The carrier does not drive that slot.
    NotDriven,
    /// The carrier's I/O expander did not answer.
    Failed,
}

/// A bounded queue whose depth the firmware reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum QueueId {
    /// The C6's queue toward the P4. Counts bytes.
    RadioUp,
    /// The P4's queue of records waiting for a packet to the C6. Counts bytes.
    RadioDown,
    /// The P4's priority ring toward the host. Counts bytes.
    HostPriority,
    /// The P4's bulk ring toward the host. Counts bytes.
    HostBulk,
    /// The P4's queues of records for its nodes, one per slot: one slot's capacity, the
    /// deepest any slot has been, and every slot's refusals. Counts bytes.
    NodeDown,
}

impl QueueId {
    /// Every queue, in declaration order.
    pub const ALL: [Self; 5] =
        [Self::RadioUp, Self::RadioDown, Self::HostPriority, Self::HostBulk, Self::NodeDown];

    /// The name `wartui status` and a capture's `bridge_queue` rows give this queue.
    pub fn name(self) -> &'static str {
        match self {
            Self::RadioUp => "radio-up",
            Self::RadioDown => "radio-down",
            Self::HostPriority => "host-priority",
            Self::HostBulk => "host-bulk",
            Self::NodeDown => "node-down",
        }
    }

    /// What this queue's capacity, high-water mark and drops count: `"bytes"` or `"entries"`.
    pub fn unit(self) -> &'static str {
        match self {
            Self::RadioUp
            | Self::RadioDown
            | Self::HostPriority
            | Self::HostBulk
            | Self::NodeDown => "bytes",
        }
    }

    /// Whether this queue lives on the P4, whose counts run on through a C6 restart. The rest are
    /// the C6's and restart with it.
    pub fn on_p4(self) -> bool {
        match self {
            Self::RadioDown | Self::HostPriority | Self::HostBulk | Self::NodeDown => true,
            Self::RadioUp => false,
        }
    }

    /// Whether this queue's drops are summed into [`BridgeToHost::Status`]'s `dropped_tx`: the
    /// queues a frame bound for the host passes through.
    pub fn in_dropped_tx(self) -> bool {
        match self {
            Self::RadioUp | Self::HostPriority | Self::HostBulk => true,
            Self::RadioDown | Self::NodeDown => false,
        }
    }

    /// The queue [`QueueId::name`] gives `name`, or `None` for a name it never gives.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.name() == name)
    }
}

/// How full a queue has been, and what it has lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct QueueStat {
    /// Which queue this describes; it also says whether the counts are bytes or entries.
    pub queue: QueueId,
    /// Its size.
    pub capacity: u32,
    /// The most it has held at once.
    pub high_water: u32,
    /// Items evicted or refused since boot.
    pub dropped: u32,
}

/// The queues a [`BridgeToHost::Status`] reports; room for every [`QueueId`] with spare.
pub type QueueStats = Vec<QueueStat, 8>;

/// The ESP32-C6 behind a split bridge, as it described itself over UART.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RadioInfo {
    /// The radio chip.
    pub chip: Chip,
    /// The radio's MAC.
    pub mac: Mac,
    /// The radio firmware's version.
    pub fw_version: ShortStr,
    /// Why the radio's current life started.
    pub reset_cause: ResetCause,
    /// Where the radio's previous life stopped.
    pub last_phase: LoopPhase,
    /// Milliseconds since the radio started.
    pub uptime_ms: u32,
}

/// Counters of the P4–C6 UART link, since boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RadioLinkStats {
    /// Verified C6→P4 packets.
    pub packets: u32,
    /// C6→P4 packets whose CRC failed.
    pub crc_fail: u32,
    /// C6→P4 packets with the wrong magic.
    pub bad_magic: u32,
    /// C6→P4 packets whose sequence number skipped.
    pub seq_gap: u32,
    /// Line errors on the P4's UART: framing, parity, glitch or FIFO overflow.
    pub uart_errors: u32,
    /// P4→C6 packets the C6 saw fail their CRC, from its `RadioStatus`.
    pub radio_crc_fail: u32,
    /// P4→C6 packets the C6 saw with the wrong magic.
    pub radio_bad_magic: u32,
    /// P4→C6 packets the C6 saw skip a sequence number.
    pub radio_seq_gap: u32,
    /// Line errors on the C6's UART, from its `RadioStatus`.
    pub radio_uart_errors: u32,
}
