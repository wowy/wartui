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

/// Bumped whenever the message enums change shape. Host and bridge must agree.
///
/// v2 added [`HostToBridge::Identify`], because v1 announced the bridge only at
/// boot and so a host that attached to an already-running dongle waited for a
/// [`BridgeToHost::Ready`] that had been sent minutes earlier.
///
/// v3 added [`ResetCause`], [`LoopPhase`], `heap_free` and `uptime_ms` to
/// [`BridgeToHost::Ready`]. A bridge that reboots is no longer a bridge whose
/// last life is a mystery: it now says whether it was powered on, panicked,
/// was asked to reset, or gave up on a transmit path that had stopped
/// draining — and where in its loop it was when that happened. The uptime is
/// what lets the host tell that story apart from a duplicate answer to its own
/// [`HostToBridge::Identify`], which is the only other way two `Ready` frames
/// arrive on one connection.
pub const LINK_PROTO_VERSION: u8 = 3;

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

// A frame is the version byte, the payload, and the CRC, then COBS overhead of
// one byte per 254 plus a leading marker, plus the terminator. Checking it here
// means a payload that outgrew the buffer is a build error, not a silent
// truncation on the wire at three in the morning.
const MAX_BODY: usize = MAX_FRAME - 8;
const _: () = assert!(MAX_ESPNOW_PAYLOAD + 32 < MAX_BODY);

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
/// A flattening of `esp_hal`'s per-chip `SocResetReason`, which has a dozen
/// variants that differ between the C5 and the C6 and name silicon blocks
/// rather than causes. What an operator needs is which of a small number of
/// stories this was, and the ones that matter are the ones that are not
/// [`Self::PowerOn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum ResetCause {
    /// The board was plugged in, or the button was pressed.
    PowerOn,
    /// The firmware reset itself: the panic handler, or a
    /// [`HostToBridge::Reset`].
    Software,
    /// A watchdog fired, so the main loop stopped turning over.
    Watchdog,
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
    /// The radio confirmed delivery. Unicast ESP-NOW is MAC-acknowledged, so
    /// this is real delivery, not just a successful enqueue — which is what
    /// lets the host clear an assignment's dirty flag honestly.
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

/// Commands the host sends to the bridge.
///
/// The bridge understands framing and the radio, and nothing about what the
/// bytes mean. Every rule that could be wrong lives on the host, where it is
/// unit-testable and does not need a reflash to change.
// The payload variants dwarf the rest, but boxing them would mean an allocator
// in the bridge firmware, which is exactly what this crate avoids. These values
// are transient — built, serialized and dropped — never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum HostToBridge {
    /// Ask the bridge to announce itself with [`BridgeToHost::Ready`].
    ///
    /// The host sends this the moment it opens the port. Without it the only
    /// announcement is the one at boot, so restarting the TUI without also
    /// unplugging the dongle would leave the host waiting forever for a frame
    /// that had already been sent and read.
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
    /// Drop a peer registration. Never issued as a side effect of sending —
    /// doing that is the firmware bug at `src/WiFiOps.cpp:676`.
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
}

/// Events and replies the bridge sends to the host.
// The payload variants dwarf the rest, but boxing them would mean an allocator
// in the bridge firmware, which is exactly what this crate avoids. These values
// are transient — built, serialized and dropped — never stored in bulk.
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
        /// Bytes free in the radio blobs' heap. Nothing wartui writes
        /// allocates, so a figure that falls across a long capture is the
        /// blobs leaking and is worth knowing before the allocation fails.
        heap_free: u32,
        /// Milliseconds since this life started, at the moment of announcing.
        ///
        /// Carried so the host can tell a *new* life from a second answer to
        /// an [`HostToBridge::Identify`] it sent twice. Both arrive on the
        /// same connection — a software reset does not re-enumerate the USB
        /// device, so the host's file descriptor reads straight through the
        /// reboot — and nothing else in this frame separates them: two
        /// consecutive `wartui reset`s produce byte-identical `Ready`s. An
        /// uptime that went *backwards* is a reboot and cannot be anything
        /// else.
        uptime_ms: u32,
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
        /// Bridge-local microsecond timestamp. Paired with the one on
        /// [`Self::SendResult`], this measures the heartbeat-to-assignment
        /// latency without the host's own scheduling noise confusing matters.
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
    /// Diagnostics. Routed through the link rather than printed, because
    /// `esp-println` would interleave into the same USB endpoint and corrupt
    /// the framing.
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
