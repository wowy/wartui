//! The link between the host and the P4 bridge: USB to the board's CH343, then UART0 at
//! [`HOST_BAUD`] to the P4, or Bluetooth through the board's C6 ([`crate::ble`]).
//!
//! A frame is a version byte, a body and a checksum, framed so a reader resynchronizes from
//! arbitrary junk:
//!
//! ```text
//! COBS( version:u8 || body || crc16:u16le ) || 0x00
//! ```
//!
//! - `version` is [`FRAME_VERSION`], 2. It names the frame, not the messages. A frame carrying 1
//!   comes from a build without the frozen core.
//! - `crc16` is CRC-16/CCITT-FALSE ([`crc16`]) over `version || body`. It rejects what the UART
//!   leg corrupts, which nothing retries, half-written frames, and the bootloader's log a reset
//!   prints down the same pipe.
//! - The body's first byte is the message's **code**, numbered per direction.
//! - A `0x00` ends a frame and may come at any time: an empty frame is nothing. A host leads a
//!   connection's first frame with one.
//!
//! **The skip rule.** A frame a side cannot read is dropped, and the conversation goes on: a COBS
//! or CRC failure, another `version`, a code it does not know, or a body it cannot decode. No
//! answer is owed and no state changes. [`decode_frame`] and [`decode_host_frame`] return
//! `Ok(None)` for a sound frame whose code this side does not read.
//!
//! # The frozen core
//!
//! **Frozen forever, even before 1.0**, so any later host can reach, identify, update and restart
//! any bridge that carries it:
//!
//! - the line: [`HOST_BAUD`], 8N1, no flow control, and a host that never moves DTR or RTS, which
//!   are the board's reset circuit; over Bluetooth, the service, the write rules and the address
//!   in [`crate::ble`];
//! - the frame above and the skip rule. A frame carrying a frozen message is at most
//!   [`MAX_FRAME`] bytes on the wire, and every bridge and host takes one that long
//!   ([`MAX_HOST_FRAME`]);
//! - the five messages below, laid out by hand and read before any postcard, so neither an enum
//!   reorder nor a postcard change moves them. A reader ignores bytes after the body it knows, and
//!   these codes are never reused;
//! - the handshake, the update conversation and what an update leaves in place, below.
//!
//! | Direction | Code | Message | Body after the code |
//! |---|---|---|---|
//! | host to bridge | 0 | [`HostToBridge::Identify`] | none |
//! | host to bridge | 2 | [`HostToBridge::Reset`] | none. The bridge reboots, and answers nothing |
//! | host to bridge | 4 | [`HostToBridge::Update`] | a LEB128 length, then one op, at most [`OP_MAX`] bytes |
//! | bridge to host | 0 | [`BridgeToHost::Ready`] | [`Ready`]'s layout |
//! | bridge to host | 5 | [`BridgeToHost::Update`] | a LEB128 length, then one `Status` op, at most [`UPDATE_STATUS_MAX`] bytes |
//!
//! Everything else changes between builds: every other message both ways, and the rest of
//! `Ready`. The node image upload ([`HostToBridge::CacheImage`]) is one: it carries the frozen
//! ops under an unfrozen code. So are pairing and the bonds' record layout, the notice a central
//! gets when USB takes the bridge, and each side's own timing. An unfrozen code belongs to the
//! build that sent it, and means what that build's major says.
//!
//! # What the version means
//!
//! [`ReadyCore::version`] is the P4 firmware's (`firmware/p4-bridge/Cargo.toml`), and follows
//! semantic versioning. The rule is frozen with the field, so a later host reads an older
//! bridge's version by it:
//!
//! | Part | A bump means | Two ends that differ in it |
//! |---|---|---|
//! | major | an unfrozen message's bytes changed, or a message went | share the core only: the host reaches, identifies, updates and restarts the bridge, and nothing else |
//! | minor | a new message, at the next code | speak every message both know; a code one end does not know is skipped |
//! | patch | no message changed | speak the same messages |
//!
//! Within a major a message's bytes never change, and no code is reused or renumbered. That
//! covers the rest of `Ready` and every type a message carries: a new field or enum value is a
//! major, since postcard cannot read a message shorter or wider than its type. The host's own
//! version is the bridge firmware its tree builds ([`BRIDGE_VERSION`]), which it never sends: a
//! bridge needs only the skip rule. [`Standing`] is the rule as a value.
//! `crates/wartui-proto/tests/link_messages.txt` holds every message's bytes, so a change without
//! its bump fails a test.
//!
//! # The handshake
//!
//! - A bridge writes nothing to a USB host until a host frame it decodes arrives, and nothing to
//!   a central until it subscribes.
//! - A host sends `Identify` when it opens the link, and may send it at any time. A bridge that
//!   knows its identity answers each `Identify` with a `Ready`. One still waiting on its C6 at
//!   boot answers nothing yet, so a host asks until one answers.
//! - A bridge also sends a `Ready` unasked, when a central subscribes and when a new life begins.
//!   A host admits nothing that arrives ahead of a connection's first `Ready`.
//! - **`Identify` is the frame that always counts.** It attaches a USB host, keeps it attached,
//!   and confirms an image on trial. A bridge serves a USB host for [`HOST_SILENCE_SECS`] after
//!   its last decoded frame, so a host sends a frozen frame at least that often. A central stays
//!   attached while it stays subscribed.
//! - **USB outranks a central.** Over USB an `Identify` is answered without taking the queue from
//!   a central. An `Update` or a `Reset` takes it, so a wired host always gets in.
//!
//! # The update conversation
//!
//! Each `Update` carries one op in [`crate::radio_link::update`]'s layout, which is frozen with
//! the radio link's core, and the P4 answers with `Status` ops. Every refusal is a `Status`. An
//! op a bridge does not know is skipped with no answer: it names no session to answer for. A
//! session ends on a taken `Install`, an `Abort`, a `Begin` for another image, a reboot, or the
//! host going: the USB gate closing, or the central letting go.
//!
//! The image is an app image for the P4's spare app slot, 0x7F0000 bytes on every bridge that
//! carries the core, with its appended SHA-256 digest. A longer one is refused `TooLarge`. The
//! P4's image carries its C6's, and the P4 updates its C6.
//!
//! A bridge boots a new image on trial, and refuses `Begin` while on it. Once its C6 has answered,
//! a host frame it decodes confirms the image, and `Identify` is one. The third unconfirmed boot
//! returns to the image before. `Ready` names the outcome: its `image` is the booted image's
//! digest, and its `trial` says whether that image is confirmed, on trial, or failed its confirm.
//!
//! # What an update leaves in place
//!
//! An update writes the spare app slot, `otadata` and the `trial` sector, and nothing else. So
//! every later image runs on the bootloader and partition table of the bridge's last wired flash
//! (`firmware/p4-bridge/partitions.csv`), finds its slot and `trial` sector by `locate`'s rule in
//! `firmware/common/src/ota.rs`, reads the sector in [`crate::ota::trial`]'s layout, and leaves
//! the `nvs` partition's two sectors, the settings and the bonds, as it found them.
//!
//! # The rest of the link
//!
//! The P4 plans the fleet ([`crate::fleet`]): the host asks it to start or stop scanning
//! ([`HostToBridge::StartScan`], [`HostToBridge::StopScan`]) and mirrors the plan it reports
//! ([`BridgeToHost::Fleet`]). The P4 authors every `Assign` and `Stop` itself.
//!
//! The host speaks to a node by its node-bus [`Slot`]: a [`HostToBridge::ToNode`] carries one
//! record for it, of up to [`MAX_NODE_DOWN`] bytes, and the P4 answers each with
//! [`BridgeToHost::Delivered`] or [`BridgeToHost::Undelivered`]. Every record a node sends
//! arrives as a [`BridgeToHost::FromNode`] of up to [`MAX_NODE_PAYLOAD`] bytes. The P4 reads a
//! heartbeat for its planner and forwards it verbatim, and reads nothing else; [`crate::bus`] has
//! the messages.
//!
//! An update's chunks are the exception: the host uploads the image once into the P4's cache
//! ([`HostToBridge::CacheImage`]), and a [`HostToBridge::ToNodeChunk`] asks the P4 to build each
//! [`crate::bus::UpdateChunk`] from it ([`cache_chunk_body`]), answered like a
//! [`HostToBridge::ToNode`].

use crate::bus::{DieTemp, MAX_SLOTS, Slot};
use crate::fan::{FanDuty, FanStats};
use crate::fleet::{Member, Report, ScanMode};
use crate::mac::Mac;
use crate::plan::{ChannelPool, PlanSettings};
use crate::radio_link::update::OP_MAX;
use crate::radio_link::{BleCounters, RadioHealth};
use crate::reset::ResetCause;
use heapless::{String, Vec};
use serde::Serialize;

mod bytes;
mod cache;
mod frame;
mod ready;

pub use cache::{NodeImageCache, cache_chunk_body, image_tag};
pub(crate) use frame::CobsStream;
pub use frame::{
    BRIDGE_TO_HOST_CODES, FrameAccumulator, HOST_TO_BRIDGE_CODES, LinkError, Reads, crc16,
    decode_frame, decode_host_frame, encode_frame, encode_host_frame,
};
pub use ready::{
    BRIDGE_VERSION, End, READY_CORE_LEN, Ready, ReadyCore, ReadyRest, Standing, Trial,
};

/// The host link's baud rate, 8N1, set at both ends.
///
/// The fastest rate the CH343 carries cleanly, and one the P4 divides exactly from its 40 MHz
/// crystal; `docs/p4-host-uart-findings.md` has the sweep.
pub const HOST_BAUD: u32 = 4_000_000;

/// The frame's version byte: 2, forever. It names the frame, not the messages, which
/// [`BRIDGE_VERSION`] names.
///
/// Not 1: a frame from a build without the frozen core carries 1, and its `Ready` puts the first
/// octet of its address where `core_len` sits, so some addresses would pass as a frozen `Ready`.
pub const FRAME_VERSION: u8 = 2;

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

/// One encoded [`crate::radio_link::update::Op`] for a P4 bridge's own image or its node image
/// cache, in transit to it.
pub type UpdateOp = Vec<u8, OP_MAX>;

/// The most bytes one [`BridgeToHost::Update`] or [`BridgeToHost::CacheImage`] carries: an
/// encoded `Op::Status`, with room for its body to grow at the end.
pub const UPDATE_STATUS_MAX: usize = 16;

/// One encoded `Op::Status`, in transit to the host.
pub type UpdateStatusBytes = Vec<u8, UPDATE_STATUS_MAX>;

/// The P4's [`BridgeToHost::Error`] while no GPS receiver answers on its port, repeated with
/// every announcement. The simulator sends it too, as a P4 with nothing on the port.
pub const NO_GPS_RECEIVER: &str = "no GPS receiver answered on the I2C bus (GPIO7/GPIO8)";

/// The P4's [`BridgeToHost::Error`] when its GPS port's bus refuses its configuration, repeated
/// with every announcement. The host reads it as [`NO_GPS_RECEIVER`]; with a constant
/// configuration it is near impossible.
pub const GPS_BUS_REFUSED: &str = "the GPS port's I2C bus refused its configuration";

/// The P4's [`BridgeToHost::Log`] when a GPS receiver answers on its port after
/// [`NO_GPS_RECEIVER`]. The host reads these texts to tell whether the bridge has a receiver.
pub const GPS_RECEIVER_ANSWERED: &str = "the GPS receiver answered on the I2C bus (GPIO7/GPIO8)";

/// The start of the P4's [`BridgeToHost::Error`] while it updates its C6
/// (`split::Pair::fault`). `wartui flash-bridge` follows the C6's update by it.
pub const RADIO_UPDATING: &str = "updating the radio";

/// The P4's [`BridgeToHost::Error`] to a Bluetooth central when a USB host takes the host queue
/// from it (`crate::host_owner`). The host transport reads it as the link going down, not as a
/// fault: the central stays subscribed, and the queue comes back to it with a fresh `Ready`.
pub const USB_TOOK_BRIDGE: &str = "the USB host took the bridge";

/// What the first line of the C6's report of its last panic starts with
/// (`firmware/p4-radio/src/last_panic.rs`). The P4 keeps that line and repeats it to every host
/// that attaches: the C6 reports once, at its boot, when the host it restarted under has
/// usually gone.
pub const RADIO_PANICKED: &str = "C6 panicked: ";

/// Split `text` over as many [`LogStr`] lines as it takes, the first led by `first` and each
/// after it by `more`, and give each line to `each`. Cuts only at a character's end. A prefix
/// that leaves no room for a character ends the split.
///
/// A bridge chip's last panic, which can run past one line, goes to the host this way
/// (`firmware/p4-radio/src/last_panic.rs`, `firmware/p4-bridge/src/sd_log.rs`).
pub fn split_lines(first: &str, more: &str, text: &str, mut each: impl FnMut(LogStr)) {
    let mut rest = text;
    let mut prefix = first;
    loop {
        let mut line = LogStr::new();
        let _ = line.push_str(prefix);
        let mut taken = 0;
        for c in rest.chars() {
            if line.push(c).is_err() {
                break;
            }
            taken += c.len_utf8();
        }
        each(line);
        rest = rest.get(taken..).unwrap_or_default();
        if rest.is_empty() || taken == 0 {
            break;
        }
        prefix = more;
    }
}

/// What a bridge chip's firmware last marked before it reset.
///
/// Carried across a reset in RTC memory and reported in [`ReadyRest::last_phase`],
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

/// Why the P4 could not deliver a [`HostToBridge::ToNode`] or a [`HostToBridge::ToNodeChunk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Undelivered {
    /// No node answers in that slot.
    NoNode,
    /// The slot's queue had no room.
    QueueFull,
    /// The node did not acknowledge the transfer that carried it, after every try.
    Timeout,
    /// The P4's cache holds no verified image with the chunk's tag, or the chunk runs past that
    /// image's end.
    NoImage,
}

/// Commands the host sends to the bridge.
///
/// The bridge understands framing, and of what a node's bytes mean only heartbeats, which its
/// planner reads, and the [`crate::bus::UpdateChunk`] records it builds from its cache.
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
    /// wait forever. Frozen: code 0, no body.
    Identify,
    /// Ask for a [`BridgeToHost::Status`].
    GetStatus,
    /// Reboot the bridge. Frozen: code 2, no body.
    Reset,
    /// Bytes to write verbatim to the GPS receiver, composed by the host.
    ///
    /// The bridge does not read them, so a change to the receiver's configuration costs
    /// a `cargo run`, not a reflash.
    GpsWrite {
        /// The receiver's own protocol, typically UBX.
        #[serde(with = "bytes")]
        bytes: GpsWriteBytes,
    },
    /// One step of an update to a P4 bridge's own image: a `Begin`, `Chunk`, `Commit`,
    /// `Install` or `Abort` in the [`crate::radio_link::update`] layout, which the P4 answers
    /// with a [`BridgeToHost::Update`]. The P4 handles the same bytes its C6 does, so one codec
    /// serves both links. Every refusal is a `Status`. Frozen: code 4, laid out by hand.
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
    /// One step of an upload into the P4's node image cache: a `Begin`, `Chunk` or `Commit` in
    /// the [`crate::radio_link::update`] layout, which the P4 answers with a
    /// [`BridgeToHost::CacheImage`]. The cache serves an image only once its digest checks.
    CacheImage(#[serde(with = "bytes")] UpdateOp),
    /// Send the node in `slot` an [`crate::bus::UpdateChunk`] of `len` bytes at `offset` from
    /// the cached image tagged `image` ([`image_tag`]), answered by [`BridgeToHost::Delivered`]
    /// or [`BridgeToHost::Undelivered`] as a [`HostToBridge::ToNode`] is.
    ToNodeChunk {
        /// Where the node sits.
        slot: Slot,
        /// Correlates the answer.
        id: u16,
        /// The cached image's [`image_tag`].
        image: u32,
        /// The node's update session.
        session: u8,
        /// Where in the image the chunk starts.
        offset: u32,
        /// Image bytes in the chunk, 1 to [`crate::bus::UPDATE_CHUNK`].
        len: u16,
    },
    /// Run the carrier's lid fan at `duty`, and store it so the P4 boots into it
    /// ([`crate::fan`]). No answer: the next [`BridgeToHost::Status`] shows the duty in force,
    /// and a failed save answers [`BridgeToHost::Error`]. A P4 with no carrier ignores it.
    SetFan {
        /// The duty to run and store.
        duty: FanDuty,
    },
    /// Run the channel pool `pool`, and store it so the P4 boots into it
    /// ([`crate::settings`]). No answer: the next [`BridgeToHost::Status`] shows the pool in
    /// force, and a failed save answers [`BridgeToHost::Error`]. Every P4 takes it, carrier or
    /// not.
    SetPool {
        /// The pool to run and store.
        pool: ChannelPool,
    },
    /// Scan Wi-Fi, and Bluetooth too when `bluetooth` ([`crate::fleet::Fleet::start`]). The P4
    /// keeps scanning until a [`HostToBridge::StopScan`], whether or not a host is listening. No
    /// answer: a [`BridgeToHost::Fleet`] reports the plan once it changes.
    StartScan {
        /// Give one node the Bluetooth scan, once two nodes are members.
        bluetooth: bool,
    },
    /// Idle every node ([`crate::fleet::Fleet::stop`]). No answer, as for
    /// [`HostToBridge::StartScan`].
    StopScan,
    /// Open the Bluetooth pairing window for [`PAIR_WINDOW_SECS`], so a new central can pair and
    /// bond ([`crate::bonds`]). No answer: the next [`BridgeToHost::Status`] shows the window.
    OpenPairing,
    /// Drop every stored Bluetooth bond, which opens the pairing window until a central bonds.
    /// No answer, as for [`HostToBridge::OpenPairing`]; a failed erase answers
    /// [`BridgeToHost::Error`].
    ClearBonds,
    /// Offer the WardriveESP service to phones when `on`, and store it so the P4 boots into it
    /// ([`crate::wardrive_esp`]). No answer: the next [`BridgeToHost::Status`] shows the setting
    /// in force, and a failed save answers [`BridgeToHost::Error`].
    SetWardriveEsp {
        /// Whether phones may link.
        on: bool,
    },
    /// Format the bridge's microSD card as FAT32, erasing it ([`crate::sd::format`]), answered
    /// by [`BridgeToHost::SdFormatted`]. The next [`BridgeToHost::Status`] shows the card.
    FormatSd,
    /// Log to the microSD card when `on`, and store it so the P4 boots into it; unset, the P4
    /// logs. No answer: the next [`BridgeToHost::Status`] shows the setting in force
    /// ([`SdStatus::log`]), and a failed save answers [`BridgeToHost::Error`].
    SetSdLog {
        /// Whether the bridge logs.
        on: bool,
    },
    /// Deal with `planner`, and store it so the P4 boots into it ([`crate::settings`]). No
    /// answer: the next [`BridgeToHost::Status`] shows the planner in force, and a failed save
    /// answers [`BridgeToHost::Error`]. Every P4 takes it, carrier or not.
    SetPlanner {
        /// The planner and sparse node count to run and store.
        planner: PlanSettings,
    },
}

/// How long [`HostToBridge::OpenPairing`] opens the pairing window.
pub const PAIR_WINDOW_SECS: u16 = 120;

/// How long after the last USB frame the P4 stops serving USB and hands the queue back to a
/// waiting Bluetooth host: three of the engine's 5 s status polls.
pub const HOST_SILENCE_SECS: u16 = 15;

/// Events and replies the bridge sends to the host.
// Unboxed for the reason `HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum BridgeToHost {
    /// Sent in answer to [`HostToBridge::Identify`], when a host's first frame arrives, and
    /// when a new life begins while a host is present (`firmware/p4-bridge/src/host.rs`).
    /// Frozen: code 0, in [`Ready`]'s layout.
    Ready(Ready),
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
        /// Fill level of each bounded queue: the P4's, and the C6's once it has reported them.
        queues: QueueStats,
        /// The P4–C6 UART link's counters.
        radio_link: RadioLinkStats,
        /// Host-link reads since boot that saw one or more line errors or overflows on the
        /// P4's host UART: FIFO overflows, framing, parity and glitches. One read counts once
        /// however many it saw. An overflow is host bytes lost, as when the P4 holds its
        /// interrupts off for its own flash work.
        host_uart_errors: u32,
        /// The carrier's lid fan, or `None` with no carrier.
        fan: Option<FanStats>,
        /// The channel pool the bridge holds and its planner cuts.
        pool: ChannelPool,
        /// The planner the bridge deals with, and its sparse node count.
        planner: PlanSettings,
        /// What the bridge's planner is asked to scan.
        scan: ScanMode,
        /// The slot the bridge's planner gave the Bluetooth scan, or `None` for no node.
        ble: Option<Slot>,
        /// Bit n is set while the P4 counts the slot whose [`Slot`] byte is n present. A slot is
        /// present after one good transfer, and absent after five in a row fail the magic, once
        /// a second has passed since the last good one (`firmware/p4-bridge/src/bus.rs`). A u32,
        /// so a board with more slots than [`crate::bus::MAX_SLOTS`] fits without a wire change.
        present: u32,
        /// Every slot the bridge's planner counts a member, with the MAC its heartbeat named, in
        /// slot order ([`crate::fleet::Fleet::members`]). A sighting batch names no node, and the
        /// host drops one from a slot it cannot name after the bridge acknowledged it, so the
        /// host files each member here as well as from its heartbeats, and a run's first `Status`
        /// names every slot before the scan starts. A `Status` is a priority frame and can
        /// overtake batches queued before it, so it fills only a slot nothing has named; a
        /// heartbeat, which travels in order with the batches, stays the authority.
        members: Vec<Member, MAX_SLOTS>,
        /// The microSD card, or `None` when the bridge has no card slot or does not drive it.
        sd: Option<SdStatus>,
        /// The Bluetooth host link: who holds the host queue, the bonds and pairing window, the
        /// connected central and the C6's counters.
        bluetooth: BluetoothStatus,
        /// The bridge's own chips: the P4's die temperature, and the C6's latest health.
        health: BridgeHealth,
    },
    /// Diagnostics, routed through the link rather than printed. `esp-println` would
    /// interleave into the same endpoint and corrupt the framing.
    Log {
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
    /// [`crate::radio_link::update`] layout, answering a [`HostToBridge::Update`]. Frozen: code
    /// 5, laid out by hand.
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
    /// The node acknowledged the transfer that carried [`HostToBridge::ToNode`] or
    /// [`HostToBridge::ToNodeChunk`] `id`.
    Delivered {
        /// Echoes the request's `id`.
        id: u16,
    },
    /// [`HostToBridge::ToNode`] or [`HostToBridge::ToNodeChunk`] `id` did not reach its node.
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
    /// Where an upload into the node image cache stands: an `Op::Status` in the
    /// [`crate::radio_link::update`] layout, answering a [`HostToBridge::CacheImage`].
    CacheImage(#[serde(with = "bytes")] UpdateStatusBytes),
    /// What the bridge's planner is doing: sent when it changes, after each
    /// [`BridgeToHost::Status`], and when a host attaches. A priority frame, so it reaches the
    /// host ahead of the heartbeats that show nodes adopting it.
    Fleet(Report),
    /// The answer to [`HostToBridge::FormatSd`]. A priority frame.
    SdFormatted {
        /// How it went.
        outcome: SdFormatOutcome,
    },
}

/// The Bluetooth host link, as a [`BridgeToHost::Status`] reports it. The default is no host,
/// no bond, the window closed and no central.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct BluetoothStatus {
    /// Which host the P4's host queue serves.
    pub owner: HostOwner,
    /// Bonds the P4 holds ([`crate::bonds::MAX_BONDS`] at most).
    pub bonds: u8,
    /// Whether a central that holds no bond may pair.
    pub window: PairWindow,
    /// The connected central, or `None` with none.
    pub central: Option<Central>,
    /// The C6's counters, totals since it booted.
    pub counters: BleCounters,
    /// Whether the bridge offers the WardriveESP service to phones: the setting, not whether a
    /// phone is linked.
    pub wardrive_esp: bool,
}

/// The bridge's own chips, as a [`BridgeToHost::Status`] reports them. The default is no
/// reading from either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct BridgeHealth {
    /// The P4's die temperature, read when the `Status` was built ([`crate::tsens`]).
    pub p4_die_temp: DieTemp,
    /// The C6's latest [`crate::radio_link::Up::Health`], or `None` until this C6 life has sent
    /// one.
    pub radio: Option<RadioHealth>,
}

/// Which host the P4's host queue serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum HostOwner {
    /// No host: the P4 queues nothing.
    #[default]
    None,
    /// A host on USB, through the CH343.
    Usb,
    /// A bonded central over Bluetooth, through the C6.
    Bluetooth,
}

impl HostOwner {
    /// The owner's name, as `wartui status` prints it and a capture stores it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
        }
    }
}

/// The Bluetooth pairing window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum PairWindow {
    /// Only a bonded central is served.
    #[default]
    Closed,
    /// A new central may pair for `secs` more seconds.
    Open {
        /// Seconds left.
        secs: u16,
    },
    /// No bond is stored, so the window stays open until a central bonds.
    UntilBonded,
}

/// The central connected to the C6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Central {
    /// The link is encrypted.
    pub encrypted: bool,
    /// The central holds a stored bond.
    pub bonded: bool,
    /// The ATT MTU.
    pub mtu: u16,
    /// The PHY: 1 for 1M, 2 for 2M, 3 for Coded.
    pub phy: u8,
    /// The connection interval, in microseconds.
    pub interval_us: u32,
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

/// How a [`HostToBridge::FormatSd`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum SdFormatOutcome {
    /// The card holds a fresh FAT32 volume.
    Done,
    /// No card answered.
    NoCard,
    /// A format is already running.
    Busy,
    /// The card refused a write, or is too small or too large for the layout.
    Failed,
}

/// The microSD card, as a [`BridgeToHost::Status`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SdStatus {
    /// What the card holds.
    pub state: SdState,
    /// The card's size in MiB, 0 with no card.
    pub card_mb: u32,
    /// The volume's free space in MiB, when it is [`SdState::Ready`] and its FSInfo sector says.
    pub free_mb: Option<u32>,
    /// The log the bridge writes to the card.
    pub log: SdLog,
}

impl SdStatus {
    /// A card in `state`, with the log [`SdLog::OFF`].
    #[must_use]
    pub const fn card(state: SdState, card_mb: u32, free_mb: Option<u32>) -> Self {
        Self { state, card_mb, free_mb, log: SdLog::OFF }
    }
}

/// The log the bridge writes to its microSD card, as [`SdStatus::log`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SdLog {
    /// The setting [`HostToBridge::SetSdLog`] stores.
    pub on: bool,
    /// What the writer is doing.
    pub state: SdLogState,
    /// The number of the file open, `LOG/NNNNNNNN.BIN`, or `None` with none open.
    pub file: Option<u32>,
    /// Bytes written to the card since boot.
    pub written: u64,
    /// Records dropped since boot, because the log's ring filled before the card took them.
    pub dropped: u32,
    /// Card transfers that failed since boot.
    pub errors: u32,
    /// The longest synchronous stretch of one writer step, in microseconds: a check that the
    /// log never stalls the bridge's other work.
    pub busy_max_us: u32,
}

impl SdLog {
    /// The setting off, and nothing written.
    pub const OFF: Self = Self {
        on: false,
        state: SdLogState::Off,
        file: None,
        written: 0,
        dropped: 0,
        errors: 0,
        busy_max_us: 0,
    };
}

/// What the bridge's SD log writer is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum SdLogState {
    /// The setting is off.
    Off,
    /// The setting is on, and no writable card is in yet, or the writer is paused.
    Waiting,
    /// A file is open.
    Writing,
    /// The card has no room above the reserve, and deleting the old logs cannot restore it.
    Full,
    /// The card's volume is one the writer will not touch.
    Refused,
    /// The writer stopped on a card error or a corrupt volume, until the card is identified
    /// again, formatted, or the setting toggled.
    Failed,
}

/// What the bridge's microSD card holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum SdState {
    /// No card answered.
    NoCard,
    /// A card answered and holds no FAT32 volume the bridge reads.
    NoFat32,
    /// The card holds a FAT32 volume.
    Ready,
    /// A format is running.
    Formatting,
    /// The card answered, then failed a read or the last format.
    Failed,
}

/// A bounded queue whose depth the firmware reports. Every queue counts bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum QueueId {
    /// The C6's queue toward the P4.
    RadioUp,
    /// The P4's queue of records waiting for a packet to the C6.
    RadioDown,
    /// The P4's priority ring toward the host.
    HostPriority,
    /// The P4's bulk ring toward the host.
    HostBulk,
    /// The P4's queues of records for its nodes, one per slot: one slot's capacity, the
    /// deepest any slot has been, and every slot's refusals.
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
    /// Which queue this describes.
    pub queue: QueueId,
    /// Its size, in bytes.
    pub capacity: u32,
    /// The most it has held at once.
    pub high_water: u32,
    /// Items evicted or refused since boot.
    pub dropped: u32,
}

/// The queues a [`BridgeToHost::Status`] reports; room for every [`QueueId`] with spare.
pub type QueueStats = Vec<QueueStat, 8>;

/// The ESP32-C6 behind the P4, as it described itself over UART.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RadioInfo {
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
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
