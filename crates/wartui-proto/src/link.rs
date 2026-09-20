//! The USB link between the host and the bridge dongle.
//!
//! This is wartui's own protocol, not the firmware's, so it gets the discipline
//! the on-air format lacks: an explicit version byte, a checksum, and framing
//! that resynchronises from arbitrary junk.
//!
//! ```text
//! COBS( version:u8 || postcard(message) || crc16:u16le ) || 0x00
//! ```
//!
//! The version byte sits *outside* the postcard blob so a mismatched bridge
//! flash is detectable without a successful deserialize — which is exactly the
//! situation where deserializing cannot be trusted. The CRC is not there for
//! line integrity, since USB bulk transfers are already checked and retried in
//! hardware; it is there to reject half-written frames and the ROM bootloader
//! banner that a reset sprays down the same pipe.

use heapless::{String, Vec};
use serde::{Serialize, de::DeserializeOwned};

/// The revision of this protocol both ends must agree on.
///
/// Held at 1 until 1.0, whatever the message enums do, and for the reason
/// [`crate::air::WIRE_VERSION`] is: nothing before 1.0 is compatible with an earlier
/// wartui and the policy is to flash both ends from one tree, so there is no older peer
/// for the byte to protect. It is the lever kept for the first change a build in the field
/// has to survive, and spending it on a shape change nobody can still be running would
/// leave nothing to spend then.
///
/// The cost is real and belongs where it will be read. The byte sits *outside* the postcard
/// blob so a mismatched flash is detectable without a successful deserialize, and holding it
/// gives that up: a host and a bridge built from different trees meet as an undecodable
/// frame rather than a named mismatch. Postcard writes an enum variant as its index and a
/// struct's fields in order, so a new variant is a byte with no case and a new field on
/// [`BridgeToHost::Ready`] shifts everything after it — met inside the very frame meant to
/// introduce the bridge, which reads as a bridge that answered nothing. That is also why
/// every addition to these enums goes on the end.
pub const LINK_PROTO_VERSION: u8 = 1;

/// ESP-NOW's own payload ceiling. The 212-byte wardriver frames fit inside it.
pub const MAX_ESPNOW_PAYLOAD: usize = 250;

/// Buffer size both ends allocate for one encoded frame.
pub const MAX_FRAME: usize = 512;

/// A MAC address.
pub type Mac = [u8; 6];

/// An ESP-NOW payload in transit over USB.
pub type EspNowPayload = Vec<u8, MAX_ESPNOW_PAYLOAD>;

/// Short identifier, such as a firmware version.
pub type ShortStr = String<32>;

/// A log line from the bridge.
pub type LogStr = String<96>;

/// Broadcast address. Nodes send heartbeats and observations here in plaintext
/// mode, so the bridge hears them without any peer registration.
pub const BROADCAST: Mac = [0xFF; 6];

// A frame is the version byte, the payload and the CRC, then COBS overhead of one
// byte per 254 plus a leading marker, plus the terminator. Checked here so a
// payload that outgrew the buffer is a build error rather than a silent truncation.
const MAX_BODY: usize = MAX_FRAME - 8;
const _: () = assert!(MAX_ESPNOW_PAYLOAD + 32 < MAX_BODY);
// The other frame with a size worth checking. A whole-panel push carries every row every
// time, so the worst case is fixed rather than traffic-dependent: `PANEL_ROWS` lines of a
// full `ShortStr`, each with a severity byte and postcard's length prefix, plus the
// variant index and the vector's own length. Checked here so a panel that grew a row or a
// wider `ShortStr` is a build error rather than a bridge quietly losing the bottom of its
// screen.
const _: () = assert!(
    PANEL_ROWS * (32 + 2) + 2 < MAX_BODY,
    "a whole-panel push must fit one frame, or a bridge would silently lose rows"
);

/// Which chip the bridge firmware is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Chip {
    /// Dual-band, same radio as the nodes.
    Esp32C5,
    /// 2.4 GHz only, which is all ESP-NOW needs at the default channel.
    Esp32C6,
}

/// Why the bridge is running this life rather than the last one.
///
/// A flattening of `esp_hal`'s per-chip `SocResetReason`, which names silicon
/// blocks rather than causes and differs between the two parts. What an operator
/// needs is which story this was, and the ones that matter are not [`Self::PowerOn`].
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
    /// Reported by the C5 alone, and not to be folded into
    /// [`ResetCause::Watchdog`]: no watchdog on any of these parts actually fires
    /// (`docs/phase-3-findings.md`), so `Watchdog` here would name a mechanism
    /// known not to work. The corollary, worth stating where it will be read: on a
    /// C6 the hang class has *no* signal and the board has to be unplugged.
    Lockup,
    /// The supply sagged. Usually a hub or a cable rather than the board.
    Brownout,
    /// A reset the firmware did not ask for and cannot attribute, which
    /// includes the one `espflash` drives over DTR/RTS.
    External,
    /// The chip reported something this build does not have a name for.
    Unknown,
}

/// Where the bridge's main loop was when it last stopped making progress.
///
/// Carried across a reset in RTC memory and reported in
/// [`BridgeToHost::Ready`], because the interesting resets are the ones nobody
/// was watching. On its own it is a hint rather than a diagnosis — the loop
/// visits most of these every millisecond — but paired with a
/// [`ResetCause::Watchdog`] or [`ResetCause::Software`] it says which of the
/// blocking calls in the loop was the one that did not come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum LoopPhase {
    /// Nothing to report: a power-on, or a reset that did not preserve the
    /// marker.
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
    /// The transmit path stopped draining while the host was still talking, so
    /// the bridge reset itself. See [`BridgeToHost::Ready`].
    TxStalled,
    /// Pushing pixels at the panel, which is the other call in the loop that
    /// blocks for longer than a memcpy.
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
    /// The radio confirmed delivery. Unicast ESP-NOW is MAC-acknowledged, so this
    /// is real delivery rather than a successful enqueue.
    AckOk,
    /// The frame went out but no acknowledgement came back.
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

/// The most rows a [`Panel`] may have, and so the most lines one
/// [`HostToBridge::ShowPanel`] carries.
///
/// A ceiling rather than a count: what a bridge actually has depends on the font it
/// draws in, it says so in [`BridgeToHost::Ready`], and a panel that reports fewer is
/// sent fewer. Eight leaves room above any font that fits five lines on the 80-pixel
/// screen this was written for, and the frame is sized against it.
pub const PANEL_ROWS: usize = 8;

/// A panel the bridge can draw lines of text on.
///
/// Announced in [`BridgeToHost::Ready`] rather than configured, so the host formats to the
/// geometry that is actually there and sends nothing at all to a bridge without a screen.
/// That is also why there is no operator flag for any of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Panel {
    /// Characters that fit across one line.
    pub cols: u8,
    /// Lines that fit down the screen, never more than [`PANEL_ROWS`].
    pub rows: u8,
}

/// How a line is going.
///
/// The bridge maps this to a colour and the host decides which one a line has, because the
/// thresholds are the host's to know: what counts as a weak link is arithmetic over a
/// snapshot, and retuning it must not cost a reflash. The colours themselves are a property
/// of the panel and live in the firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Severity {
    /// Fine.
    Ok,
    /// Working, but not as it should be.
    Warn,
    /// A fault, and the capture is the worse for it.
    Error,
}

/// One row of the panel, laid out by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PanelLine {
    /// How the thing this line reports is going.
    pub level: Severity,
    /// The text, already truncated to the panel's width.
    pub text: ShortStr,
}

/// A whole panel's worth of lines.
pub type PanelLines = Vec<PanelLine, PANEL_ROWS>;

/// Commands the host sends to the bridge.
///
/// The bridge understands framing and the radio, and nothing about what the bytes
/// mean; see `firmware/bridge/src/main.rs`.
// The payload variants dwarf the rest, but boxing them would mean an allocator
// in the bridge firmware, which is exactly what this crate avoids. These values
// are transient — built, serialized and dropped — never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum HostToBridge {
    /// Ask the bridge to announce itself with [`BridgeToHost::Ready`].
    ///
    /// Sent the moment the host opens the port: without it the only announcement
    /// is the one at boot, so restarting the TUI without unplugging the dongle
    /// would wait forever for a frame already sent.
    Identify,
    /// Park the radio on an ESP-NOW channel. The stock mesh uses 6.
    SetChannel {
        /// Wi-Fi channel number.
        channel: u8,
    },
    /// Register a peer so unicast frames can be addressed to it.
    AddPeer {
        /// Peer address.
        mac: Mac,
    },
    /// Drop a peer registration. Never issued as a side effect of sending, which
    /// would race the transmit callback for the frame just sent.
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
        /// Register the peer first if it is not already known. Add-if-absent
        /// only; it never removes anything.
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
    /// Every line, every time, so a push is idempotent: a bridge that reboots mid-session
    /// repaints correctly on the next one with no resync protocol to get wrong. At the
    /// host's redraw rate that costs a few hundred bytes a second, which is not worth
    /// trading that property for.
    ///
    /// The host composes the text and decides each line's [`Severity`]; the bridge blits
    /// what it is handed. That is what keeps the bridge format-blind, and what makes a
    /// change to what the panel says cost a `cargo run` rather than a reflash.
    ShowPanel {
        /// One per row, top to bottom. Never more than the [`Panel`] announced.
        lines: PanelLines,
    },
}

/// Events and replies the bridge sends to the host.
// Unboxed for the reason `HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum BridgeToHost {
    /// Sent at startup and in answer to [`HostToBridge::Identify`]. The host
    /// checks `proto_version` and refuses to continue against a bridge it does
    /// not understand.
    Ready {
        /// Which chip this is.
        chip: Chip,
        /// The bridge's own MAC, which nodes will see as the core's address.
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
        /// Bytes free in the radio blobs' heap. Nothing wartui writes allocates, so
        /// a figure that falls across a long capture is the blobs leaking.
        heap_free: u32,
        /// Milliseconds since this life started, at the moment of announcing.
        ///
        /// Carried so the host can tell a *new* life from a second answer to an
        /// [`HostToBridge::Identify`] it sent twice: a software reset does not
        /// re-enumerate the USB device, so both arrive on one connection and
        /// nothing else in the frame separates them. See
        /// `crates/wartui-bridge/src/serial.rs`.
        uptime_ms: u32,
        /// The screen this bridge has, if it has one.
        ///
        /// The bridge advertising its own geometry is what removes the operator flag: a
        /// board with no panel reports `None` and is sent no [`HostToBridge::ShowPanel`]
        /// at all, and one with a panel is formatted to the width it really has rather
        /// than to a number the host guessed.
        panel: Option<Panel>,
    },
    /// An ESP-NOW frame arrived.
    Rx {
        /// Transmitting node.
        src: Mac,
        /// Destination, usually [`BROADCAST`].
        dst: Mac,
        /// Signal strength in dBm.
        rssi: i8,
        /// Channel the radio was parked on.
        channel: u8,
        /// Bridge-local microsecond timestamp. Against the one on
        /// [`Self::SendResult`] this measures the heartbeat-to-assignment latency
        /// without the host's own scheduling noise.
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
        /// Channel currently parked on.
        channel: u8,
        /// Registered peers.
        peer_count: u8,
        /// Frames received since boot.
        rx_count: u32,
        /// Frames the outbound ring dropped because the host was not draining
        /// it. Non-zero means the host fell behind.
        dropped_tx: u32,
        /// Bridge uptime.
        uptime_ms: u32,
    },
    /// Diagnostics, routed through the link rather than printed: `esp-println`
    /// would interleave into the same endpoint and corrupt the framing.
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

/// Why a frame could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The output buffer was too small for the encoded frame.
    BufferTooSmall,
    /// COBS decoding failed; the frame was corrupt.
    Corrupt,
    /// The frame was shorter than a version byte plus a CRC.
    TooShort,
    /// CRC mismatch — a partial write, or line noise after a reset.
    BadChecksum {
        /// What the frame claimed.
        expected: u16,
        /// What the bytes actually hash to.
        actual: u16,
    },
    /// The peer speaks a different revision of this protocol.
    VersionMismatch {
        /// What this build speaks.
        ours: u8,
        /// What arrived.
        theirs: u8,
    },
    /// The body was not a valid message.
    Malformed,
}

impl core::fmt::Display for LinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BufferTooSmall => f.write_str("buffer too small for encoded frame"),
            Self::Corrupt => f.write_str("COBS decode failed"),
            Self::TooShort => f.write_str("frame shorter than its own header"),
            Self::BadChecksum { expected, actual } => {
                write!(f, "checksum mismatch: expected {expected:#06x}, got {actual:#06x}")
            }
            Self::VersionMismatch { ours, theirs } => {
                write!(f, "link protocol mismatch: we speak v{ours}, peer speaks v{theirs}")
            }
            Self::Malformed => f.write_str("frame body was not a valid message"),
        }
    }
}

impl core::error::Error for LinkError {}

/// CRC-16/CCITT-FALSE: polynomial `0x1021`, initial value `0xFFFF`, no
/// reflection and no final XOR.
///
/// Implemented here rather than pulled in as a dependency: it is a dozen lines,
/// and the bridge firmware's dependency budget is worth defending.
#[must_use]
pub const fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    let mut i = 0;
    while i < data.len() {
        crc ^= (data[i] as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// Serialize `msg` into `out` as a complete frame, terminator included.
///
/// Returns how many bytes of `out` were used.
///
/// # Errors
/// [`LinkError::BufferTooSmall`] if `out` cannot hold the encoded frame.
pub fn encode_frame<T: Serialize>(msg: &T, out: &mut [u8]) -> Result<usize, LinkError> {
    let mut body = [0u8; MAX_BODY];
    body[0] = LINK_PROTO_VERSION;
    let used =
        postcard::to_slice(msg, &mut body[1..]).map_err(|_| LinkError::BufferTooSmall)?.len();
    let end = 1 + used;
    if end + 2 > MAX_BODY {
        return Err(LinkError::BufferTooSmall);
    }
    let crc = crc16(&body[..end]);
    body[end..end + 2].copy_from_slice(&crc.to_le_bytes());

    let encoded = cobs::try_encode(&body[..end + 2], out).map_err(|_| LinkError::BufferTooSmall)?;
    // COBS output never contains a zero, so the terminator is unambiguous.
    *out.get_mut(encoded).ok_or(LinkError::BufferTooSmall)? = 0x00;
    Ok(encoded + 1)
}

/// Decode a COBS-encoded frame body, with the terminating zero already removed.
///
/// # Errors
/// See [`LinkError`].
pub fn decode_frame<T: DeserializeOwned>(frame: &mut [u8]) -> Result<T, LinkError> {
    let len = cobs::decode_in_place(frame).map_err(|_| LinkError::Corrupt)?;
    if len < 3 {
        return Err(LinkError::TooShort);
    }
    let body = &frame[..len];
    let (payload, checksum) = body.split_at(len - 2);
    let expected = u16::from_le_bytes([checksum[0], checksum[1]]);
    let actual = crc16(payload);
    if expected != actual {
        return Err(LinkError::BadChecksum { expected, actual });
    }
    if payload[0] != LINK_PROTO_VERSION {
        return Err(LinkError::VersionMismatch { ours: LINK_PROTO_VERSION, theirs: payload[0] });
    }
    postcard::from_bytes(&payload[1..]).map_err(|_| LinkError::Malformed)
}

/// Reassembles frames from a byte stream.
///
/// Both ends use this. A zero byte ends a frame, so the accumulator recovers on
/// its own from a reset banner, a half-written frame or an unplugged cable: the
/// junk is discarded at the next terminator and the stream carries on.
#[derive(Debug)]
pub struct FrameAccumulator<const N: usize = MAX_FRAME> {
    buf: [u8; N],
    len: usize,
    overflowed: bool,
}

impl<const N: usize> Default for FrameAccumulator<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FrameAccumulator<N> {
    /// An empty accumulator.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0u8; N], len: 0, overflowed: false }
    }

    /// Discard any partial frame.
    pub const fn reset(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }

    /// Feed one byte.
    ///
    /// Returns the raw COBS-encoded frame when a terminator completes one; pass
    /// it to [`decode_frame`]. Empty frames and frames that overran the buffer
    /// yield `None`, having resynchronised.
    pub fn push(&mut self, byte: u8) -> Option<&mut [u8]> {
        if byte != 0x00 {
            if self.len < N {
                self.buf[self.len] = byte;
                self.len += 1;
            } else {
                self.overflowed = true;
            }
            return None;
        }

        let len = self.len;
        let overflowed = self.overflowed;
        self.reset();
        if overflowed || len == 0 {
            // A run of zeros, or a frame too big to be ours. Either way the
            // next terminator gives us a clean start.
            return None;
        }
        Some(&mut self.buf[..len])
    }
}
