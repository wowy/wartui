//! The on-air ESP-NOW frames.
//!
//! Little-endian, no CRC. Encoding and decoding are written out by hand rather
//! than transmuting a `#[repr(packed)]` struct: the byte layout is a contract
//! between two separately compiled programs, so it deserves to be spelled out
//! and tested against real bytes.
//!
//! Every frame in both directions is wartui's own, and is deliberately
//! unrecognisable to the vendor firmware this project grew up against. ESP-NOW
//! has no addressing above the MAC layer and a node broadcasts to
//! `FF:FF:FF:FF:FF:FF`, so anything speaking the vendor's format  on the
//! control channel is in everybody's conversation at once.
//!
//! A magic of our own solves it. It is checked before anything else on both
//! ends, so a vendor frame costs one `memcmp` here and ours costs one there.
//! [`foreign`] is what remains of vendor awareness: it recognises `ENOW` in
//! order to *report* it, because another fleet on the control channel is worth
//! saying out loud, and never decodes a byte of it.
//!
//! The header carries a version, which the vendor's did not. It is the lever
//! for the next incompatible change: a node speaking a version this host does
//! not know is counted and named rather than half-decoded

use core::fmt;

use crate::plan::{CHANNEL_SET_BYTES, ChannelSet};

/// Frame preamble, `"WTUI"`. Four raw bytes, not a NUL-terminated string.
pub const MAGIC: [u8; 4] = *b"WTUI";

/// The frame version this build speaks and the only one it decodes.
///
/// Bumped when any layout below changes shape. Anything else is reported as
/// incompatible rather than guessed at — see the module docs.
pub const WIRE_VERSION: u8 = 1;

/// Longest SSID 802.11 allows, and so the most a sighting carries.
pub const SSID_MAX: usize = 32;

/// Length of [`HeartbeatMsg`] on the wire.
pub const HEARTBEAT_MSG_LEN: usize = OFF_BODY + 7;

/// Length of [`AdminMsg`] on the wire.
pub const ADMIN_MSG_LEN: usize = OFF_BODY + 4 + CHANNEL_SET_BYTES;

/// Length of a [`SightingMsg`] carrying no SSID — the floor a decoder needs
/// before it can read `ssid_len` and find out how much more there is.
pub const SIGHTING_MSG_MIN: usize = OFF_BODY + 11;

/// Length of a [`SightingMsg`] carrying the longest SSID there is, and so a
/// buffer [`SightingMsg::encode_into`] can always finish in.
pub const SIGHTING_MSG_MAX: usize = SIGHTING_MSG_MIN + SSID_MAX;

/// [`AdminMsg::flags`] bit 0: scan Bluetooth as well as Wi-Fi.
///
/// Off is the safe default and the one a node boots into whatever its build
/// says, because the coexistence cost is real and measured
/// (`docs/phase-0-findings.md`): on a stock node BLE cost every one of the
/// thirty-two assignments sent to it. The `ble` cargo feature decides only
/// whether the code is compiled in; this bit decides whether it runs.
pub const ADMIN_FLAG_BLE: u8 = 1 << 0;

/// [`Capabilities::flags`] bit 0: this build has the Bluetooth scan compiled in.
pub const CAP_FLAG_BLE: u8 = 1 << 0;

/// [`Capabilities::flags`] bit 1: this radio reaches 5 GHz.
pub const CAP_FLAG_5G: u8 = 1 << 1;

const OFF_VERSION: usize = 4;
const OFF_TYPE: usize = 5;
const OFF_BODY: usize = 6;

/// The header alone: enough to know whether a frame is ours and what shape it
/// claims to be.
const HEADER_LEN: usize = OFF_BODY;

/// What a frame is. Deliberately not the vendor's `1..=5`, and with the
/// direction in the high bit so a misrouted frame is a decode error rather than
/// a plausible one of something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgType {
    /// Node → core, once per completed channel sweep.
    Heartbeat = 0x01,
    /// Node → core, one per newly-seen BSSID.
    Sighting = 0x02,
    /// Core → node. The only frame a node acts on.
    Admin = 0x81,
}

impl MsgType {
    /// The discriminant as it appears on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for MsgType {
    type Error = DecodeError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0x01 => Ok(Self::Heartbeat),
            0x02 => Ok(Self::Sighting),
            0x81 => Ok(Self::Admin),
            other => Err(DecodeError::UnknownType(other)),
        }
    }
}

/// Why a byte slice was not a valid frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the message type requires.
    TooShort {
        /// Bytes required for this message type.
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// First four bytes were not [`MAGIC`].
    BadMagic,
    /// A frame of ours, from a build speaking a version this one does not.
    /// Reported rather than guessed at: the whole point of the version byte is
    /// that a layout change must not decode as a plausible older frame.
    BadVersion(u8),
    /// Type byte named no frame this build knows.
    UnknownType(u8),
    /// `ssid_len` exceeded [`SSID_MAX`], which 802.11 makes impossible, so the
    /// frame is malformed rather than merely unusual.
    SsidTooLong(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { need, got } => write!(f, "frame too short: need {need}, got {got}"),
            Self::BadMagic => f.write_str("bad magic, expected \"WTUI\""),
            Self::BadVersion(v) => write!(f, "wire version {v}, expected {WIRE_VERSION}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:#04x}"),
            Self::SsidTooLong(n) => write!(f, "ssid length {n} exceeds {SSID_MAX}"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Check the header and return the type it names.
///
/// Every decoder starts here, so magic, version and type are rejected in the
/// same order and with the same errors wherever a frame arrives.
fn header(buf: &[u8]) -> Result<MsgType, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::TooShort { need: HEADER_LEN, got: buf.len() });
    }
    if buf[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if buf[OFF_VERSION] != WIRE_VERSION {
        return Err(DecodeError::BadVersion(buf[OFF_VERSION]));
    }
    MsgType::try_from(buf[OFF_TYPE])
}

/// Write the header for `msg_type` into the front of `out`.
fn write_header(out: &mut [u8], msg_type: MsgType) {
    out[..4].copy_from_slice(&MAGIC);
    out[OFF_VERSION] = WIRE_VERSION;
    out[OFF_TYPE] = msg_type.as_u8();
}

/// What a node says it is, in every heartbeat it sends.
///
/// This used to be an ASCII token squeezed into the vendor's text field,
/// because that field was the only unused space on the wire and the only way to
/// tell one of ours from a stock node whose bytes were otherwise identical. The
/// magic answers that question now, so what is left is the part that was always
/// load-bearing: a version, and which of two optional capabilities this
/// particular board has.
///
/// Both features are refusals rather than requests. A node without `ble` is
/// never handed [`ADMIN_FLAG_BLE`], because it would acknowledge the assignment
/// and scan nothing; a node without `five_ghz` is never dealt a 5 GHz index,
/// because a share it cannot tune is a share nobody scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Bumped when node → core changes shape in a way an older host cannot
    /// read. Distinct from [`WIRE_VERSION`], which covers the frame around it.
    pub major: u8,
    /// Bumped for additions an older host can ignore.
    pub minor: u8,
    /// Built with the `ble` cargo feature, so the Bluetooth scan is code this
    /// node actually has. Says nothing about whether it is running: that is the
    /// assignment's [`ADMIN_FLAG_BLE`], and it is off at every boot.
    pub ble: bool,
    /// The radio reaches 5 GHz. False on an ESP32-C6, which is 2.4 GHz only.
    pub five_ghz: bool,
}

/// The node protocol version this build speaks, written into every heartbeat.
pub const CAPABILITY_MAJOR: u8 = 1;
/// See [`CAPABILITY_MAJOR`].
pub const CAPABILITY_MINOR: u8 = 0;

impl Capabilities {
    /// What this build is, for a node to announce.
    #[must_use]
    pub const fn here(ble: bool, five_ghz: bool) -> Self {
        Self { major: CAPABILITY_MAJOR, minor: CAPABILITY_MINOR, ble, five_ghz }
    }

    /// The feature bits as the wire carries them.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        let mut flags = 0;
        if self.ble {
            flags |= CAP_FLAG_BLE;
        }
        if self.five_ghz {
            flags |= CAP_FLAG_5G;
        }
        flags
    }

    /// Rebuild from the three bytes a heartbeat carries.
    ///
    /// Unknown flag bits are ignored rather than refused, because a node from a
    /// later build must not stop being a node just because it can do something
    /// this host has never heard of.
    #[must_use]
    pub const fn from_parts(major: u8, minor: u8, flags: u8) -> Self {
        Self { major, minor, ble: flags & CAP_FLAG_BLE != 0, five_ghz: flags & CAP_FLAG_5G != 0 }
    }
}

/// The form the fleet table and the store column show, and the one the old
/// ASCII token had: `wartui/1.0;ble,5g`.
impl fmt::Display for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wartui/{}.{}", self.major, self.minor)?;
        let mut first = true;
        for (present, name) in [(self.ble, "ble"), (self.five_ghz, "5g")] {
            if !present {
                continue;
            }
            f.write_str(if first { ";" } else { "," })?;
            f.write_str(name)?;
            first = false;
        }
        Ok(())
    }
}

/// Node → core, once per completed channel sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatMsg {
    /// Monotonic from node boot, so a value below the last one means the node
    /// restarted and has forgotten whatever assignment it held.
    pub counter: u32,
    /// What that node is.
    pub capabilities: Capabilities,
}

impl HeartbeatMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Heartbeat => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() < HEARTBEAT_MSG_LEN {
            return Err(DecodeError::TooShort { need: HEARTBEAT_MSG_LEN, got: buf.len() });
        }
        let counter = u32::from_le_bytes([
            buf[OFF_BODY],
            buf[OFF_BODY + 1],
            buf[OFF_BODY + 2],
            buf[OFF_BODY + 3],
        ]);
        Ok(Self {
            counter,
            capabilities: Capabilities::from_parts(
                buf[OFF_BODY + 4],
                buf[OFF_BODY + 5],
                buf[OFF_BODY + 6],
            ),
        })
    }

    /// Encode to the thirteen bytes a heartbeat is.
    #[must_use]
    pub fn encode(&self) -> [u8; HEARTBEAT_MSG_LEN] {
        let mut out = [0u8; HEARTBEAT_MSG_LEN];
        write_header(&mut out, MsgType::Heartbeat);
        out[OFF_BODY..OFF_BODY + 4].copy_from_slice(&self.counter.to_le_bytes());
        out[OFF_BODY + 4] = self.capabilities.major;
        out[OFF_BODY + 5] = self.capabilities.minor;
        out[OFF_BODY + 6] = self.capabilities.flags();
        out
    }
}

/// What a [`SightingMsg`] observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// A Wi-Fi access point.
    Wifi = 0,
    /// A BLE advertiser.
    Ble = 1,
}

impl RecordKind {
    /// The discriminant as it appears on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for RecordKind {
    type Error = DecodeError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::Wifi),
            1 => Ok(Self::Ble),
            other => Err(DecodeError::UnknownType(other)),
        }
    }
}

/// What a network's information elements amount to.
///
/// One byte on the wire, and the WiGLE `AuthMode` spelling only at the edge
/// where WiGLE wants it — [`Display`](fmt::Display), which the store row and
/// the exported column both go through. The vendor put the spelling on the
/// wire, which cost seventy bytes a record to carry a closed set of eleven
/// values and made the parser on this end a string comparison.
///
/// The set is open-ended, so a discriminant this build does not know is carried
/// through as [`Security::Unknown`] rather than failing the frame: a node from
/// a later build must not lose an observation to a security mode this host has
/// never heard of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Security {
    Open,
    Wep,
    WpaPsk,
    Wpa2Psk,
    WpaWpa2Psk,
    /// `[WPA2]`, which is `WIFI_AUTH_WPA2_ENTERPRISE` despite the name.
    Wpa2Enterprise,
    Wpa3Psk,
    Wpa2Wpa3Psk,
    WapiPsk,
    Undefined,
    /// The placeholder the BLE path reports.
    Ble,
    /// A discriminant this build did not have when it was written.
    Unknown(u8),
}

impl Security {
    /// The discriminant as it appears on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Wep => 1,
            Self::WpaPsk => 2,
            Self::Wpa2Psk => 3,
            Self::WpaWpa2Psk => 4,
            Self::Wpa2Enterprise => 5,
            Self::Wpa3Psk => 6,
            Self::Wpa2Wpa3Psk => 7,
            Self::WapiPsk => 8,
            Self::Undefined => 9,
            Self::Ble => 10,
            Self::Unknown(raw) => raw,
        }
    }

    /// The inverse. Never yields [`Security::Unknown`] holding a value one of
    /// the named variants already has, so `as_u8` and `from_u8` round-trip.
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Open,
            1 => Self::Wep,
            2 => Self::WpaPsk,
            3 => Self::Wpa2Psk,
            4 => Self::WpaWpa2Psk,
            5 => Self::Wpa2Enterprise,
            6 => Self::Wpa3Psk,
            7 => Self::Wpa2Wpa3Psk,
            8 => Self::WapiPsk,
            9 => Self::Undefined,
            10 => Self::Ble,
            other => Self::Unknown(other),
        }
    }

    /// The WiGLE `AuthMode` token, for everything but [`Security::Unknown`].
    #[must_use]
    pub const fn token(self) -> Option<&'static str> {
        Some(match self {
            Self::Open => "[OPEN]",
            Self::Wep => "[WEP]",
            Self::WpaPsk => "[WPA_PSK]",
            Self::Wpa2Psk => "[WPA2_PSK]",
            Self::WpaWpa2Psk => "[WPA_WPA2_PSK]",
            Self::Wpa2Enterprise => "[WPA2]",
            Self::Wpa3Psk => "[WPA3_PSK]",
            Self::Wpa2Wpa3Psk => "[WPA2_WPA3_PSK]",
            Self::WapiPsk => "[WAPI_PSK]",
            Self::Undefined => "[UNDEFINED]",
            Self::Ble => "[BLE]",
            Self::Unknown(_) => return None,
        })
    }
}

impl fmt::Display for Security {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.token() {
            Some(token) => f.write_str(token),
            // Said in a shape that still reads as an AuthMode token and still
            // says which one, so an export from an older host is a lead rather
            // than a shrug.
            None => write!(f, "[UNKNOWN:{}]", self.as_u8()),
        }
    }
}

/// Node → core, one per newly-seen BSSID.
///
/// Seventeen bytes plus the SSID, against the vendor's fixed 212 — a
/// comma-separated line in a frame padded to `sizeof` whatever the sender's
/// compiler laid out, whether it carried two hundred bytes or thirty. That
/// padding was paid on the control channel every node shares, once per access
/// point, for a format neither end wanted.
///
/// The SSID is length-prefixed, which is the other thing that changes here. The
/// vendor line was split on commas, so a comma inside an SSID took the record
/// apart and the sender rewrote it as an underscore before transmitting — the
/// real name lost at the one point in the path where it still existed. A length
/// needs no escaping, and the CSV quoting the exporter already does is enough
/// at the only boundary that is actually CSV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SightingMsg<'a> {
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
    /// The observed BSSID or advertiser address, six raw bytes.
    pub bssid: [u8; 6],
    /// Wi-Fi channel number, or 0 for BLE.
    pub channel: u8,
    /// Signal strength in dBm, as the node measured it.
    pub rssi: i8,
    /// What its information elements amounted to.
    pub security: Security,
    /// Raw SSID bytes, at most [`SSID_MAX`]. Empty for a hidden network and
    /// always empty for BLE; not necessarily UTF-8, because an SSID is whatever
    /// the access point beaconed.
    ///
    /// This layer is byte-transparent in both directions and does no trimming
    /// of its own — a cloaked access point's zero padding is stripped where the
    /// beacon is parsed (`beacon::visible_ssid`), so that it never reaches the
    /// wire at all rather than being tidied up at each end.
    pub ssid: &'a [u8],
}

impl<'a> SightingMsg<'a> {
    /// Decode from a received frame, borrowing the SSID from it.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Sighting => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() < SIGHTING_MSG_MIN {
            return Err(DecodeError::TooShort { need: SIGHTING_MSG_MIN, got: buf.len() });
        }
        let ssid_len = buf[OFF_BODY + 10];
        if usize::from(ssid_len) > SSID_MAX {
            return Err(DecodeError::SsidTooLong(ssid_len));
        }
        let need = SIGHTING_MSG_MIN + usize::from(ssid_len);
        if buf.len() < need {
            return Err(DecodeError::TooShort { need, got: buf.len() });
        }
        let mut bssid = [0u8; 6];
        bssid.copy_from_slice(&buf[OFF_BODY + 1..OFF_BODY + 7]);
        Ok(Self {
            kind: RecordKind::try_from(buf[OFF_BODY])?,
            bssid,
            channel: buf[OFF_BODY + 7],
            // Two's complement, so the cast is the reinterpretation we want.
            #[allow(clippy::cast_possible_wrap)]
            rssi: buf[OFF_BODY + 8] as i8,
            security: Security::from_u8(buf[OFF_BODY + 9]),
            ssid: &buf[SIGHTING_MSG_MIN..need],
        })
    }

    /// Write the frame into `out`, returning how many bytes it took.
    ///
    /// `None` if `out` is too small or the SSID is longer than [`SSID_MAX`];
    /// [`SIGHTING_MSG_MAX`] is always enough. Nothing is written when it fails,
    /// so a caller cannot broadcast a half-formed frame.
    #[must_use]
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        if self.ssid.len() > SSID_MAX {
            return None;
        }
        let len = SIGHTING_MSG_MIN + self.ssid.len();
        let out = out.get_mut(..len)?;
        write_header(out, MsgType::Sighting);
        out[OFF_BODY] = self.kind.as_u8();
        out[OFF_BODY + 1..OFF_BODY + 7].copy_from_slice(&self.bssid);
        out[OFF_BODY + 7] = self.channel;
        // Two's complement again; `to_le_bytes` on an `i8` is the same byte.
        out[OFF_BODY + 8] = self.rssi.to_le_bytes()[0];
        out[OFF_BODY + 9] = self.security.as_u8();
        // The cast cannot lose data: bounded by SSID_MAX, which is 32.
        #[allow(clippy::cast_possible_truncation)]
        {
            out[OFF_BODY + 10] = self.ssid.len() as u8;
        }
        out[SIGHTING_MSG_MIN..len].copy_from_slice(self.ssid);
        Some(len)
    }
}

/// wartui's channel assignment — the one frame a node acts on.
///
/// Fifteen bytes: the header, then [`epoch`](Self::epoch), `node_index`,
/// `node_count`, [`flags`](Self::flags) and five bytes of [`ChannelSet`].
///
/// The mask is why this is not a pair of bounds. A run cannot describe a
/// restricted pool: the US pool is 2.4 GHz 1-11 and 5 GHz 36-165 with a gap
/// between, so a node holding it needed two assignments in sequence, on a dwell
/// timer, with coverage intermittent in between. A mask says it in one frame,
/// and lets the planner deal channels round-robin so every node carries some of
/// both bands.
///
/// Everything below is why the *delivery* of this frame is shaped the way it
/// is, and none of it changed with the layout. It is measured vendor behaviour,
/// kept because it is the reason for a design decision rather than because
/// anything here still interoperates.
///
/// Observed on real hardware: a vendor core's assignment to its single node
/// went out once and was retransmitted 31 times by the radio, all 32 frames
/// sharing one 802.11 sequence number with the retry bit set on all but the
/// first. Nothing acknowledged it, yet the core cleared its dirty flag from the
/// `esp_now_send` return value and moved on believing the node had been
/// assigned. Broadcast heartbeats and observations from the same node in the
/// same capture each carried their own sequence number and were never retried,
/// so the retries are specific to unicast. Acknowledgements were then captured
/// directly: of 9505 seen on the channel, none named the core, so the node
/// genuinely never answered.
///
/// A later capture found the cause. With two nodes differing only in whether
/// BLE was enabled, the BLE-off node acknowledged both assignments it was sent,
/// each transmitted once with no retry, and adopted them; the BLE-on node
/// acknowledged none of its 32 and kept scanning outside its range. An 802.11
/// acknowledgement comes from the receiver's MAC hardware, so its absence means
/// the radio was not on the channel — NimBLE shares the one 2.4 GHz antenna and
/// the admin window is precisely when the node is otherwise idle. That is the
/// measurement behind [`ADMIN_FLAG_BLE`] being a per-node decision the operator
/// makes rather than something every node does.
///
/// This is why wartui clears an assignment only on the transmit callback, why
/// it retries on the next heartbeat rather than trusting the radio's own
/// retries (all 31 of which fell inside the one failing window), and why a
/// persistently unacknowledged node should be reported to the operator as
/// likely BLE coexistence rather than as a mystery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminMsg {
    /// Epoch counter. A node adopts the assignment only when this *differs*
    /// from the one it currently holds, and never uses 0 on the wire.
    ///
    /// Distinct from the header's [`WIRE_VERSION`], which is the shape of the
    /// frame rather than the generation of the plan inside it.
    pub epoch: u8,
    /// This node's slot in the fleet-wide staggering order.
    pub node_index: u8,
    /// Fleet size the stagger is computed against.
    pub node_count: u8,
    /// Per-node switches. [`ADMIN_FLAG_BLE`] is the only one defined.
    ///
    /// Carried whole rather than unpacked into `bool`s so an unknown bit set by
    /// a newer host survives a decode and re-encode instead of being quietly
    /// dropped — the same reason [`Security::Unknown`] exists.
    pub flags: u8,
    /// Which [`SCAN_CHANNELS`](crate::plan::SCAN_CHANNELS) indices to dwell on.
    pub channels: ChannelSet,
}

impl AdminMsg {
    /// Whether this assignment asks the node to scan Bluetooth.
    #[must_use]
    pub const fn scan_ble(&self) -> bool {
        self.flags & ADMIN_FLAG_BLE != 0
    }

    /// The flags byte for a node that should or should not scan Bluetooth.
    #[must_use]
    pub const fn flags_for(scan_ble: bool) -> u8 {
        if scan_ble { ADMIN_FLAG_BLE } else { 0 }
    }

    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Admin => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() < ADMIN_MSG_LEN {
            return Err(DecodeError::TooShort { need: ADMIN_MSG_LEN, got: buf.len() });
        }
        let mut channels = [0u8; CHANNEL_SET_BYTES];
        channels.copy_from_slice(&buf[OFF_BODY + 4..OFF_BODY + 4 + CHANNEL_SET_BYTES]);
        Ok(Self {
            epoch: buf[OFF_BODY],
            node_index: buf[OFF_BODY + 1],
            node_count: buf[OFF_BODY + 2],
            flags: buf[OFF_BODY + 3],
            channels: ChannelSet::from_bytes(channels),
        })
    }

    /// Encode to the fifteen bytes a wartui node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; ADMIN_MSG_LEN] {
        let mut out = [0u8; ADMIN_MSG_LEN];
        write_header(&mut out, MsgType::Admin);
        out[OFF_BODY] = self.epoch;
        out[OFF_BODY + 1] = self.node_index;
        out[OFF_BODY + 2] = self.node_count;
        out[OFF_BODY + 3] = self.flags;
        out[OFF_BODY + 4..].copy_from_slice(&self.channels.to_bytes());
        out
    }
}

/// Turn a host-side monotonic assignment counter into the byte the wire carries.
///
/// The vendor core kept its equivalent in RAM and reset it to 1 at every boot,
/// while nodes adopt an assignment only when the byte *differs* from the one
/// they hold. Between them those two facts mean a core that restarts and
/// recomputes the same assignment is silently ignored by every node that
/// already holds it — and if the topology changed while the core was down, the
/// two views diverge permanently with nothing to say so.
///
/// wartui persists a `u64` instead and narrows it here. Zero is skipped so a
/// node holding a freshly-zeroed field cannot be mistaken for one holding an
/// assignment.
#[must_use]
pub const fn wire_epoch(counter: u64) -> u8 {
    // `% 255` lands in 0..=254; the offset moves that to 1..=255.
    ((counter.wrapping_sub(1) % 255) as u8) + 1
}

/// Any frame, dispatched on the type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// Node → core.
    Heartbeat(HeartbeatMsg),
    /// Node → core.
    Sighting(SightingMsg<'a>),
    /// Core → node. Seeing one this host did not send means another core is
    /// driving this fleet.
    Admin(AdminMsg),
}

impl<'a> Frame<'a> {
    /// Decode any frame, choosing the layout from the type byte.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Heartbeat => HeartbeatMsg::decode(buf).map(Frame::Heartbeat),
            MsgType::Sighting => SightingMsg::decode(buf).map(Frame::Sighting),
            MsgType::Admin => AdminMsg::decode(buf).map(Frame::Admin),
        }
    }
}

/// Recognising the vendor's traffic, in order to report it.
///
/// Nothing here decodes a byte. The fields are another fleet's idea of another
/// fleet and acting on them would be adopting it. But a vendor core or node on
/// the control channel is an operational fact — it is transmitting where these
/// nodes are listening, and on a stock fleet the probe requests are active
/// scans — so counting it as line noise would hide the one clue an operator has
/// for a channel that is busier than the fleet can explain.
///
/// The vendor magic is all that is matched. Since wartui's own frames no longer
/// carry it, anything that does belongs to somebody else by definition.
pub mod foreign {
    /// The vendor's frame preamble.
    pub const VENDOR_MAGIC: [u8; 4] = *b"ENOW";

    /// The vendor's `MSG_ADMIN` type byte.
    const VENDOR_ADMIN: u8 = 5;

    /// Offset of the type byte in a vendor frame.
    const VENDOR_OFF_TYPE: usize = 4;

    /// What kind of vendor frame this is, to the small extent it matters.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Foreign {
        /// Another core is assigning channels nearby. Worth its own count: it
        /// is the difference between a neighbouring fleet and a second core
        /// contending for this one.
        Admin,
        /// A vendor node's heartbeat or observation, or the encrypted-pairing
        /// frames only a node with encryption switched on ever sends.
        Node,
    }

    /// Classify a frame that is not ours, or `None` if it is not the vendor's
    /// either.
    #[must_use]
    pub fn classify(buf: &[u8]) -> Option<Foreign> {
        if buf.len() <= VENDOR_OFF_TYPE || buf[..4] != VENDOR_MAGIC {
            return None;
        }
        Some(if buf[VENDOR_OFF_TYPE] == VENDOR_ADMIN { Foreign::Admin } else { Foreign::Node })
    }
}
