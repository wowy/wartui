//! The USB link between the host and the bridge dongle.
//!
//! Unlike the air format, the link has a version byte, a checksum, and framing that
//! resynchronizes from arbitrary junk.
//!
//! ```text
//! COBS( version:u8 || postcard(message) || crc16:u16le ) || 0x00
//! ```
//!
//! The version byte sits *outside* the postcard blob, so a mismatch is detectable without
//! a deserialize that cannot be trusted. The CRC is not for line integrity: USB bulk
//! transfers are checked and retried in hardware. It rejects half-written frames, and
//! the ROM banner a reset prints down the same pipe.

use crate::mac::Mac;
use heapless::{String, Vec};
use serde::Serialize;

mod frame;
mod panel;

pub use frame::{FrameAccumulator, LinkError, crc16, decode_frame, encode_frame};
pub use panel::{PANEL_ROWS, Panel, PanelLine, PanelLines, Severity};

/// The revision of this protocol both ends must agree on.
///
/// Held at 1 until 1.0, whatever the message enums do, for the reason
/// [`crate::air::WIRE_VERSION`] is: both ends are flashed from one tree.
///
/// The cost is that a host and a bridge built from different trees meet as an
/// undecodable frame, not a named mismatch. Postcard writes an enum variant as its index
/// and a struct's fields in order. A new variant is an index the old build has no case
/// for. A new field on [`BridgeToHost::Ready`] shifts everything after it, so the bridge
/// reads as one that answered nothing. Every addition to these enums goes on the end.
pub const LINK_PROTO_VERSION: u8 = 1;

/// ESP-NOW's own payload ceiling. A [`crate::air::SightingBatch`] fills it exactly.
pub const MAX_ESPNOW_PAYLOAD: usize = 250;

/// Buffer size both ends allocate for one encoded frame.
pub const MAX_FRAME: usize = 512;

/// An ESP-NOW payload in transit over USB.
pub type EspNowPayload = Vec<u8, MAX_ESPNOW_PAYLOAD>;

/// Short identifier, such as a firmware version.
pub type ShortStr = String<32>;

/// A log line from the bridge.
pub type LogStr = String<96>;

/// Broadcast address. Nodes send heartbeats here in plaintext, so a bridge hears a node
/// before either knows the other's address. Sighting batches unicast to the bridge.
pub const BROADCAST: Mac = [0xFF; 6];

/// Which chip the bridge firmware is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Chip {
    /// Dual-band, same radio as the nodes.
    Esp32C5,
    /// 2.4 GHz only, which is all ESP-NOW needs on the control channel.
    Esp32C6,
}

/// Why the bridge is running this life rather than the last one.
///
/// A flattening of `esp_hal`'s per-chip `SocResetReason`, which names silicon blocks
/// rather than causes and differs between the two parts. An operator needs to know
/// which story this was, and the ones that matter are not [`Self::PowerOn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum ResetCause {
    /// The board was plugged in, or the button was pressed.
    PowerOn,
    /// The firmware reset itself: the panic handler, or a
    /// [`HostToBridge::Reset`].
    Software,
    /// A watchdog fired, so the main loop stopped turning over.
    Watchdog,
    /// The CPU locked up and the silicon reset it.
    ///
    /// Reported by the C5 alone. Not folded into [`ResetCause::Watchdog`], because no
    /// watchdog on these parts fires (`docs/phase-3-findings.md`). On a C6 a hang has
    /// *no* signal, and the board must be unplugged.
    Lockup,
    /// The supply sagged. Usually a hub or a cable rather than the board.
    Brownout,
    /// A reset the firmware did not ask for and cannot attribute, including the one
    /// `espflash` drives over DTR/RTS.
    External,
    /// The chip reported something this build does not have a name for.
    Unknown,
}

impl ResetCause {
    /// Whether a bridge writes to USB before any host has spoken.
    ///
    /// It does iff a host was present when the previous life ended
    /// (`host_was_present`, kept in RTC memory). Never after a [`Self::PowerOn`], where
    /// that memory is garbage.
    ///
    /// - **Why not always.** Writing to the USB Serial/JTAG endpoint with no host
    ///   reading wedges it. A C6 replugged and left unread for two minutes was dead at
    ///   first open in 3 of 3 trials, until `StallWatch` rebooted it. A build holding all
    ///   transmit until a host frame decoded was healthy in 3 of 3, answering in 2 ms
    ///   (`docs/phase-3-findings.md`). The ROM banner prints either way and is not the
    ///   cause.
    /// - **Why not never.** A host sends one `Identify` per connection. A connection
    ///   that rides through the reset hears the new life only through the unprompted
    ///   `Ready`.
    /// - **Why not by cause.** The cause says who asked for the reset, not whether
    ///   anybody was reading. A panic or `StallWatch` reset after the host left is
    ///   [`Self::Software`] with nobody there. A watchdog or lockup reset can land while
    ///   a host keeps the port open and never sends again.
    #[must_use]
    pub const fn speaks_first(self, host_was_present: bool) -> bool {
        match self {
            Self::PowerOn => false,
            Self::Software
            | Self::Watchdog
            | Self::Lockup
            | Self::Brownout
            | Self::External
            | Self::Unknown => host_was_present,
        }
    }
}

/// Where the bridge's main loop was when it last stopped making progress.
///
/// Carried across a reset in RTC memory and reported in [`BridgeToHost::Ready`],
/// because the interesting resets are the ones nobody was watching. Alone it is a hint:
/// the loop visits most phases ten times a second. Paired with a
/// [`ResetCause::Watchdog`] or [`ResetCause::Software`], it names the blocking call that
/// did not come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum LoopPhase {
    /// Nothing to report: a power-on, or a reset that did not preserve the marker.
    Unknown,
    /// Still in `main` before the loop started.
    Boot,
    /// Draining the radio's receive queue.
    DrainRadio,
    /// Reading and decoding host commands.
    DrainLink,
    /// Carrying out a host command other than a transmit.
    Command,
    /// Inside an ESP-NOW send, waiting on the transmit callback.
    Transmit,
    /// Writing queued frames to the USB endpoint.
    Pump,
    /// Idle, with neither radio nor link asking for anything.
    Idle,
    /// The transmit path stopped draining while the host was still talking, so the
    /// bridge reset itself. See [`crate::stall`].
    TxStalled,
    /// Pushing pixels at the panel, the other call in the loop that blocks for longer
    /// than a memcpy.
    Render,
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

/// What became of a [`HostToBridge::SendEspNow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum SendStatus {
    /// The radio confirmed delivery. Unicast ESP-NOW is MAC-acknowledged, so this is
    /// real delivery, not a successful enqueue.
    AckOk,
    /// The frame went out but no acknowledgment came back.
    AckFail,
    /// Broadcast, which is never acknowledged.
    Broadcast,
    /// No peer registered for the destination and `ensure_peer` was not set.
    NoPeer,
    /// The peer table is full.
    PeerTableFull,
    /// The radio refused the frame outright.
    Rejected,
}

/// Commands the host sends to the bridge.
///
/// The bridge understands framing and the radio, and nothing about what the bytes mean;
/// see `firmware/bridge/src/main.rs`.
// The payload variants dwarf the rest, but boxing them would need an allocator in the
// bridge firmware, which this crate avoids. These values are built, serialized and
// dropped, never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum HostToBridge {
    /// Ask the bridge to announce itself with [`BridgeToHost::Ready`].
    ///
    /// Sent the moment the host opens the port. A bridge that does not
    /// [speak first](ResetCause::speaks_first) says nothing until a host frame decodes.
    /// One that does announced at boot, possibly to a host long gone. Either way, a new
    /// connection that did not ask would wait forever.
    Identify,
    /// Register a peer so unicast frames can be addressed to it.
    AddPeer {
        /// Peer address.
        mac: Mac,
    },
    /// Drop a peer registration. Never a side effect of sending, which would race the
    /// transmit callback for the frame just sent.
    RemovePeer {
        /// Peer address.
        mac: Mac,
    },
    /// Transmit an ESP-NOW frame.
    SendEspNow {
        /// Correlates the eventual [`BridgeToHost::SendResult`].
        id: u16,
        /// Destination, or [`BROADCAST`].
        dst: Mac,
        /// Register the peer first if it is not already known. It never removes one.
        ensure_peer: bool,
        /// Frame bytes, already encoded by [`crate::air`].
        payload: EspNowPayload,
    },
    /// Ask for a [`BridgeToHost::Status`].
    GetStatus,
    /// Reboot the bridge.
    Reset,
    /// Lines to display, already laid out by the host.
    ///
    /// Every line, every time, so a push is idempotent. A bridge that reboots
    /// mid-session repaints correctly on the next push, with no resync protocol to get
    /// wrong. That costs a few hundred bytes a second at the host's redraw rate.
    ///
    /// The host composes the text and picks each line's [`Severity`]; the bridge blits
    /// it. A change to what the panel says then costs a `cargo run`, not a reflash.
    ShowPanel {
        /// One per row, top to bottom. Never more than the [`Panel`] announced.
        lines: PanelLines,
    },
    /// Set the bridge radio's Wi-Fi transmit power in ESP-IDF quarter-dBm units.
    ///
    /// Sent with every status poll, so a bridge that restarted under a running host is
    /// restored without operator action. The bridge reports a hardware rejection and
    /// keeps its prior setting.
    SetTxPower {
        /// ESP-IDF quarter-dBm units.
        power: i8,
    },
}

impl HostToBridge {
    /// Whether the bridge answers this command with a frame of its own.
    ///
    /// For [`crate::stall::StallWatch`], which times a transmit path that refuses bytes
    /// while a host *waits*. Only a command that asks for a reply leaves a host waiting.
    /// A command that may log or report an error does not ask: the host is not owed
    /// that answer.
    ///
    /// Exhaustive on purpose, so a new command has to say which it is.
    #[must_use]
    pub const fn asks_for_reply(&self) -> bool {
        match self {
            Self::Identify | Self::GetStatus | Self::SendEspNow { .. } => true,
            Self::AddPeer { .. }
            | Self::RemovePeer { .. }
            | Self::Reset
            | Self::ShowPanel { .. }
            | Self::SetTxPower { .. } => false,
        }
    }
}

/// Events and replies the bridge sends to the host.
// Unboxed for the reason `HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum BridgeToHost {
    /// Sent at startup when the life [speaks first](ResetCause::speaks_first), and in
    /// answer to [`HostToBridge::Identify`]. The host checks `proto_version` and refuses
    /// a bridge it does not understand.
    Ready {
        /// Which chip this is.
        chip: Chip,
        /// The bridge's own MAC, which nodes see as the host's address.
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
        /// Bytes free in the radio blobs' heap. Nothing wartui writes allocates, so a
        /// figure that falls across a long capture is the blobs leaking.
        heap_free: u32,
        /// Milliseconds since this life started, at the moment of announcing.
        ///
        /// Lets the host tell a *new* life from a second answer to an
        /// [`HostToBridge::Identify`] sent twice. A software reset does not re-enumerate
        /// the USB device, so both arrive on one connection, and nothing else in the
        /// frame separates them. See `crates/wartui-bridge/src/serial.rs`.
        uptime_ms: u32,
        /// The screen this bridge has, if it has one.
        ///
        /// `None` for a board with no screen, which is then sent no
        /// [`HostToBridge::ShowPanel`]. See [`Panel`].
        panel: Option<Panel>,
    },
    /// An ESP-NOW frame arrived.
    Rx {
        /// Transmitting node.
        src: Mac,
        /// Destination: [`BROADCAST`] for a heartbeat, the bridge's own MAC for a
        /// sighting batch.
        dst: Mac,
        /// Signal strength in dBm.
        rssi: i8,
        /// Bridge-local microsecond timestamp. Against the one on [`Self::SendResult`],
        /// it measures heartbeat-to-assignment latency without the host's scheduling
        /// noise.
        rx_us: u32,
        /// The frame.
        payload: EspNowPayload,
    },
    /// Outcome of a [`HostToBridge::SendEspNow`].
    SendResult {
        /// Echoes the request's `id`.
        id: u16,
        /// What happened.
        status: SendStatus,
        /// Bridge-local microsecond timestamp of the transmit completion.
        tx_us: u32,
    },
    /// Reply to [`HostToBridge::GetStatus`].
    Status {
        /// Registered peers.
        peer_count: u8,
        /// Frames received since boot.
        rx_count: u32,
        /// Frames the outbox dropped. See [`crate::outbox`].
        dropped_tx: u32,
        /// Bridge uptime.
        uptime_ms: u32,
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
}
