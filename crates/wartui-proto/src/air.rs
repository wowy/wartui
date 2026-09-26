//! The on-air ESP-NOW frames.
//!
//! Little-endian, no CRC. Encoding and decoding are written out by hand rather
//! than transmuting a `#[repr(packed)]` struct: the byte layout is a contract
//! between two separately compiled programs, so it deserves to be spelled out
//! and tested against real bytes.
//!
//! Every frame in both directions is wartui's own. ESP-NOW has no addressing
//! above the MAC layer and a node broadcasts to `FF:FF:FF:FF:FF:FF`, so anything
//! sharing a format on the control channel is in everybody's conversation at
//! once.
//!
//! A magic of our own solves it. It is checked before anything else, so another
//! firmware's frame costs one `memcmp`. [`foreign`] recognises one such format,
//! `ENOW`, in order to *report* it.
//!
//! The header carries a version. It is the lever held for the first change a
//! fleet in the field has to survive: a node speaking a version this host does
//! not know is counted and named rather than half-decoded. Until wartui 1.0
//! nothing is such a change, so the byte does not move — see
//! [`WIRE_VERSION`].
//!
//! A sighting never travels alone. A node packs every access point or
//! advertiser it has to report into one [`SightingBatch`], filling it to
//! [`SIGHTING_BATCH_MAX`] bytes — ESP-NOW's own payload ceiling — and sends
//! what it has at the end of a dwell or a Bluetooth scan rather than holding
//! any of it for the next one: the host stamps each record's position on
//! arrival, so a sighting held across a dwell would carry the car's next
//! position instead of the one it was heard at. [`SightingMsg`] is what one
//! record of the batch holds; [`SightingBatch::decode`] validates the whole
//! frame — every record within its limits and no trailing bytes — before
//! yielding any of it, the same "never half-decoded" rule every other frame
//! here follows.

use core::fmt;

use crate::link::MAX_ESPNOW_PAYLOAD;
use crate::plan::{CHANNEL_SET_BYTES, ChannelSet};

/// Frame preamble, `"WTUI"`. Four raw bytes, not a NUL-terminated string.
pub const MAGIC: [u8; 4] = *b"WTUI";

/// The frame version this build speaks and the only one it decodes.
///
/// 1 until wartui 1.0, whatever the layouts below do. Nothing here is
/// compatible with an earlier wartui and the fleet is flashed together, so a
/// byte telling the two apart marks a difference nothing acts on and costs a
/// re-pin of every fixture in `tests/wire.rs` to say it. This is held for the
/// first change a fleet in the field has to survive; a layout that changes
/// shape before then simply changes shape. Anything else is reported as
/// incompatible rather than guessed at — see the module docs.
pub const WIRE_VERSION: u8 = 1;

/// Longest SSID 802.11 allows, and so the most a sighting carries.
pub const SSID_MAX: usize = 32;

/// Most a sighting's trailer carries, past its length byte.
///
/// Anchored on the widest roaming consortium element anybody real beacons: a
/// count byte, a lengths byte and three five-byte identifiers — the
/// `5A03BA0000 BAA2D00000 BAA2D02000` OpenRoaming triple. A trailer that
/// would not fit is dropped whole where it is parsed rather than truncated
/// on the wire: a half-arrived identifier is nobody's.
pub const EXT_MAX: usize = 17;

/// Length of [`HeartbeatMsg`] on the wire.
pub const HEARTBEAT_MSG_LEN: usize = OFF_BODY + 7;

/// Length of [`AdminMsg`] on the wire.
pub const ADMIN_MSG_LEN: usize = OFF_BODY + 4 + CHANNEL_SET_BYTES + 1;

/// Length of [`ClearMsg`] on the wire: the header, and nothing else.
pub const CLEAR_MSG_LEN: usize = OFF_BODY;

/// Length of a [`SightingMsg`] record carrying no SSID and no trailer — the
/// floor a decoder needs before it can read `ssid_len` and find out how much
/// more there is.
///
/// A record carries no header of its own; this is the body alone, which is
/// what [`SightingBatch`] packs several of behind one.
pub const SIGHTING_RECORD_MIN: usize = 12;

/// Length of a [`SightingMsg`] record carrying the longest SSID and the
/// longest trailer there are, and so a buffer
/// [`SightingMsg::encode_record_into`] can always finish in.
pub const SIGHTING_RECORD_MAX: usize = SIGHTING_RECORD_MIN + SSID_MAX + EXT_MAX;

/// Length of a [`SightingBatch`] header: magic, version, type, `seq` and
/// `count`.
pub const SIGHTING_BATCH_HEADER: usize = OFF_BODY + 3;

/// The most a [`SightingBatch`] may be, on the wire. ESP-NOW's own payload
/// ceiling, so a batch is packed to fill exactly what one broadcast can carry.
pub const SIGHTING_BATCH_MAX: usize = 250;

const _: () = assert!(SIGHTING_BATCH_MAX == MAX_ESPNOW_PAYLOAD);

/// The most records a batch can ever carry: the header taken off the ceiling,
/// divided by the smallest a record can be. A byte budget rather than this
/// count is what [`SightingBatchWriter`] actually packs to, since records vary
/// from [`SIGHTING_RECORD_MIN`] to [`SIGHTING_RECORD_MAX`] and a count cap
/// would either overflow the frame or waste it.
pub const SIGHTINGS_PER_BATCH_MAX: usize =
    (SIGHTING_BATCH_MAX - SIGHTING_BATCH_HEADER) / SIGHTING_RECORD_MIN;

/// [`AdminMsg::flags`] bit 0: scan Bluetooth as well as Wi-Fi.
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

/// What a frame is, with the direction in the high bit so a misrouted frame is a
/// decode error rather than a plausible one of something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgType {
    /// Node → core, once per completed channel sweep.
    Heartbeat = 0x01,
    /// Node → core, every newly-seen BSSID or advertiser from one dwell or
    /// Bluetooth scan, packed into [`SightingBatch`].
    SightingBatch = 0x02,
    /// Core → node. Carries a channel and Bluetooth assignment.
    Admin = 0x81,
    /// Core → node. Asks the node to forget every address it has reported.
    Clear = 0x82,
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
            0x02 => Ok(Self::SightingBatch),
            0x81 => Ok(Self::Admin),
            0x82 => Ok(Self::Clear),
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
    /// A fixed-length frame that was not its own length, or a [`SightingBatch`]
    /// carrying bytes past its last record.
    ///
    /// [`HeartbeatMsg`] and [`AdminMsg`] are each exactly one size, so anything
    /// else carrying their type byte came from a build whose layout differs from
    /// this one's. Longer is the dangerous half: the leading bytes would parse,
    /// and the frame would be adopted as a plausible wrong assignment. Since
    /// [`WIRE_VERSION`] does not move before 1.0, this is what says a fleet is
    /// half-way through a reflash. A batch is not fixed-length, but `count`
    /// records fully accounts for its bytes or it is the same fault: a frame
    /// claiming to be something this build's layout is not.
    BadLength {
        /// Bytes this build's layout is.
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
    /// `ext_len` exceeded [`EXT_MAX`], which no parser this build knows will
    /// produce, so the frame is malformed rather than merely unusual.
    ExtTooLong(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { need, got } => write!(f, "frame too short: need {need}, got {got}"),
            Self::BadLength { need, got } => write!(f, "frame is {got} bytes, not {need}"),
            Self::BadMagic => f.write_str("bad magic, expected \"WTUI\""),
            Self::BadVersion(v) => write!(f, "wire version {v}, expected {WIRE_VERSION}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:#04x}"),
            Self::SsidTooLong(n) => write!(f, "ssid length {n} exceeds {SSID_MAX}"),
            Self::ExtTooLong(n) => write!(f, "ext length {n} exceeds {EXT_MAX}"),
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
/// A version, and which of two optional capabilities this particular board has;
/// the magic is what separates one of ours from a stock node.
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

/// The form the fleet table and the store column show: `wartui/1.0;ble,5g`. The
/// three bytes a heartbeat carries are what travels; this is for reading.
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
        if buf.len() != HEARTBEAT_MSG_LEN {
            return Err(DecodeError::BadLength { need: HEARTBEAT_MSG_LEN, got: buf.len() });
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
/// the exported column both go through. Spelling it on the wire would
/// cost seventy bytes a record to carry a handful of values, and make the parser
/// on this end a string comparison.
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

/// One record of a [`SightingBatch`]: one newly-seen BSSID or advertiser.
///
/// Twelve bytes plus the SSID and the trailer, and no padding, because every
/// byte is paid on the control channel every node shares. No header of its
/// own — [`SightingBatch`] carries one for the whole frame — so
/// [`Self::decode_record`] and [`Self::encode_record_into`] work on the record
/// alone.
///
/// The SSID is length-prefixed, so a comma or any other byte inside it arrives
/// intact. A length needs no escaping.
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
    /// The kind-dependent trailer, at most [`EXT_MAX`] bytes and
    /// length-prefixed on the wire after the SSID.
    ///
    /// For Wi-Fi it is the roaming consortium element's body verbatim — every
    /// byte between its `Element ID` and `Length` octets — and for BLE it is
    /// exactly two bytes, the Bluetooth SIG company identifier an advertiser
    /// carried, little-endian, or nothing when it carried none. One trailer
    /// rather than a field of each kind, because the two never compete for the
    /// same bytes and every byte is paid on a channel every node shares. This
    /// layer carries the bytes and does not interpret them; what they *mean* is
    /// decided where they are parsed and where they are written down.
    pub ext: &'a [u8],
}

impl<'a> SightingMsg<'a> {
    /// Decode one record from the front of `buf`, borrowing the SSID and
    /// trailer from it and returning how many bytes it took.
    ///
    /// `buf` may hold more records after this one — [`SightingBatch::decode`]
    /// is what walks them in sequence, using the returned length to find the
    /// next. A record carries no magic or version of its own; the batch
    /// around it already answered for both.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode_record(buf: &'a [u8]) -> Result<(Self, usize), DecodeError> {
        if buf.len() < SIGHTING_RECORD_MIN {
            return Err(DecodeError::TooShort { need: SIGHTING_RECORD_MIN, got: buf.len() });
        }
        let ssid_len = buf[10];
        if usize::from(ssid_len) > SSID_MAX {
            return Err(DecodeError::SsidTooLong(ssid_len));
        }
        // `SIGHTING_RECORD_MIN` already counts the trailer's length byte, so
        // this is the first byte past whatever SSID there is.
        let need = SIGHTING_RECORD_MIN + usize::from(ssid_len);
        if buf.len() < need {
            return Err(DecodeError::TooShort { need, got: buf.len() });
        }
        let ext_len = buf[need - 1];
        if usize::from(ext_len) > EXT_MAX {
            return Err(DecodeError::ExtTooLong(ext_len));
        }
        let need = need + usize::from(ext_len);
        if buf.len() < need {
            return Err(DecodeError::TooShort { need, got: buf.len() });
        }
        let mut bssid = [0u8; 6];
        bssid.copy_from_slice(&buf[1..7]);
        let msg = Self {
            kind: RecordKind::try_from(buf[0])?,
            bssid,
            channel: buf[7],
            // Two's complement, so the cast is the reinterpretation we want.
            #[allow(clippy::cast_possible_wrap)]
            rssi: buf[8] as i8,
            security: Security::from_u8(buf[9]),
            ssid: &buf[11..SIGHTING_RECORD_MIN + usize::from(ssid_len) - 1],
            ext: &buf[SIGHTING_RECORD_MIN + usize::from(ssid_len)..need],
        };
        Ok((msg, need))
    }

    /// Write the record into `out`, returning how many bytes it took.
    ///
    /// `None` if `out` is too small, the SSID is longer than [`SSID_MAX`] or
    /// the trailer is longer than [`EXT_MAX`]; [`SIGHTING_RECORD_MAX`] is
    /// always enough. Nothing is written when it fails, so a caller cannot
    /// broadcast a half-formed record — [`SightingBatchWriter::push`] relies
    /// on that to know a record did not fit rather than was half written.
    #[must_use]
    pub fn encode_record_into(&self, out: &mut [u8]) -> Option<usize> {
        if self.ssid.len() > SSID_MAX || self.ext.len() > EXT_MAX {
            return None;
        }
        let len = SIGHTING_RECORD_MIN + self.ssid.len() + self.ext.len();
        let out = out.get_mut(..len)?;
        out[0] = self.kind.as_u8();
        out[1..7].copy_from_slice(&self.bssid);
        out[7] = self.channel;
        // Two's complement again; `to_le_bytes` on an `i8` is the same byte.
        out[8] = self.rssi.to_le_bytes()[0];
        out[9] = self.security.as_u8();
        // The casts cannot lose data: bounded by SSID_MAX and EXT_MAX.
        #[allow(clippy::cast_possible_truncation)]
        {
            out[10] = self.ssid.len() as u8;
            out[11 + self.ssid.len()] = self.ext.len() as u8;
        }
        out[11..11 + self.ssid.len()].copy_from_slice(self.ssid);
        out[SIGHTING_RECORD_MIN + self.ssid.len()..len].copy_from_slice(self.ext);
        Some(len)
    }
}

/// Node → core, every newly-seen BSSID or advertiser from one dwell or
/// Bluetooth scan.
///
/// `"WTUI" | ver | 0x02 | seq:u16le | count:u8 | records…`, each record a
/// [`SightingMsg`] body. `seq` counts up once per batch a node sends; it
/// wraps, and restarts at boot the same as the heartbeat counter, so a gap in
/// it is batches lost between node and host rather than sightings lost — the
/// dedup ring already hides an individual lost sighting until its own refresh.
///
/// [`Self::decode`] validates the whole frame before anything is read out of
/// it: `count` is at least 1 and accounts for every byte with none left over,
/// and every record is within [`SIGHTING_RECORD_MIN`]/[`SIGHTING_RECORD_MAX`].
/// A fault anywhere rejects the whole frame rather than the record it is in,
/// the same "never half-decoded" rule every frame here follows. [`Self::iter`]
/// is infallible because of it — there is nothing left for a record to fail
/// on by the time anything can ask for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SightingBatch<'a> {
    /// Counts up once per batch this node has sent, wrapping and restarting at
    /// boot. A gap between two arrivals is batches this host never saw.
    pub seq: u16,
    /// How many records follow. Always at least 1: a node with nothing to
    /// report sends no batch at all.
    pub count: u8,
    /// The records, back to back with no padding between them.
    records: &'a [u8],
}

impl<'a> SightingBatch<'a> {
    /// Decode from a received frame, validating every record before returning.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::SightingBatch => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() < SIGHTING_BATCH_HEADER {
            return Err(DecodeError::TooShort { need: SIGHTING_BATCH_HEADER, got: buf.len() });
        }
        let seq = u16::from_le_bytes([buf[OFF_BODY], buf[OFF_BODY + 1]]);
        let count = buf[OFF_BODY + 2];
        let records = &buf[SIGHTING_BATCH_HEADER..];
        // A batch with nothing in it is the same fault as one that claims
        // records it does not carry: a frame this layout cannot produce.
        if count == 0 {
            return Err(DecodeError::TooShort {
                need: SIGHTING_BATCH_HEADER + SIGHTING_RECORD_MIN,
                got: buf.len(),
            });
        }
        let mut consumed = 0usize;
        for _ in 0..count {
            let (_, len) = SightingMsg::decode_record(&records[consumed..])?;
            consumed += len;
        }
        // Bytes past the last record are the batch equivalent of a
        // fixed-length frame that outgrew its layout: adopting them as
        // further records would be reading past what `count` promised.
        if consumed != records.len() {
            return Err(DecodeError::BadLength {
                need: SIGHTING_BATCH_HEADER + consumed,
                got: buf.len(),
            });
        }
        Ok(Self { seq, count, records })
    }

    /// Every record in the batch, paired with its own raw bytes.
    ///
    /// Infallible: [`Self::decode`] already walked and validated every record
    /// before this could be called.
    pub fn iter(&self) -> impl Iterator<Item = (SightingMsg<'a>, &'a [u8])> {
        let mut rest = self.records;
        let mut remaining = self.count;
        core::iter::from_fn(move || {
            if remaining == 0 {
                return None;
            }
            remaining -= 1;
            let (msg, len) =
                SightingMsg::decode_record(rest).expect("validated whole by Self::decode");
            let (raw, tail) = rest.split_at(len);
            rest = tail;
            Some((msg, raw))
        })
    }
}

/// Packs [`SightingMsg`] records into one [`SightingBatch`] frame, up to
/// [`SIGHTING_BATCH_MAX`] bytes.
///
/// Built fresh for each dwell or Bluetooth scan and sent whole at the end of
/// it — see the module docs for why nothing is ever held across one. `push`
/// writes nothing when a record would not fit, so a caller offering one more
/// than the frame can hold knows to flush what it has and start again with
/// the record that did not.
#[derive(Debug, Clone)]
pub struct SightingBatchWriter {
    buf: [u8; SIGHTING_BATCH_MAX],
    len: usize,
}

impl SightingBatchWriter {
    /// An empty batch carrying `seq`.
    #[must_use]
    pub fn new(seq: u16) -> Self {
        let mut writer = Self { buf: [0u8; SIGHTING_BATCH_MAX], len: SIGHTING_BATCH_HEADER };
        writer.reset(seq);
        writer
    }

    /// Append one record, saying whether it fit.
    ///
    /// Writes nothing when it does not: the count byte and the bytes already
    /// written are both left exactly as they were, so a caller can flush what
    /// it has and retry the same record against a fresh writer.
    #[must_use]
    pub fn push(&mut self, msg: &SightingMsg<'_>) -> bool {
        let Some(written) = msg.encode_record_into(&mut self.buf[self.len..]) else {
            return false;
        };
        self.len += written;
        self.buf[OFF_BODY + 2] += 1;
        true
    }

    /// How many records have been packed in.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.buf[OFF_BODY + 2])
    }

    /// Whether nothing has been packed in yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The frame as it stands, ready to broadcast.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Empty the writer and start a new batch carrying `seq`.
    pub fn reset(&mut self, seq: u16) {
        write_header(&mut self.buf, MsgType::SightingBatch);
        self.buf[OFF_BODY..OFF_BODY + 2].copy_from_slice(&seq.to_le_bytes());
        self.buf[OFF_BODY + 2] = 0;
        self.len = SIGHTING_BATCH_HEADER;
    }
}

/// wartui's channel assignment — one of the two frames a node acts on, the
/// other being [`ClearMsg`].
///
/// Seventeen bytes: the header, then [`epoch`](Self::epoch), `node_index`,
/// `node_count`, [`flags`](Self::flags), six bytes of [`ChannelSet`] and transmit power.
///
/// The mask is why this is not a pair of bounds. A run cannot describe a
/// restricted pool: the US pool is 2.4 GHz 1-11 and 5 GHz 36-165 with a gap
/// between, so a node holding it needed two assignments in sequence, on a dwell
/// timer, with coverage intermittent in between. A mask says it in one frame,
/// and lets the planner deal channels round-robin so every node carries some of
/// both bands.
///
/// How this frame is *delivered* is shaped by measured behaviour rather than by
/// the layout. An unacknowledged unicast is retried by the radio, all 31
/// retries falling inside the one window that failed, and an 802.11
/// acknowledgement comes from the receiver's MAC hardware — so its absence means
/// the radio was not on the channel at all, which on a stock node was NimBLE
/// holding the shared antenna (`docs/phase-0-findings.md`).
///
/// Hence three rules: an assignment is cleared only on the transmit callback,
/// retried on the next heartbeat rather than by trusting the radio's own
/// retries, and a persistently unacknowledged node is reported to the operator
/// as likely BLE coexistence rather than as a mystery.
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
    /// Wi-Fi transmit power in ESP-IDF quarter-dBm units.
    ///
    /// Applied when the assignment is adopted, alongside every other property of the
    /// node's work. The host supplies the same configured value to the bridge on
    /// connection.
    pub tx_power: i8,
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
        if buf.len() != ADMIN_MSG_LEN {
            return Err(DecodeError::BadLength { need: ADMIN_MSG_LEN, got: buf.len() });
        }
        let mut channels = [0u8; CHANNEL_SET_BYTES];
        channels.copy_from_slice(&buf[OFF_BODY + 4..OFF_BODY + 4 + CHANNEL_SET_BYTES]);
        Ok(Self {
            epoch: buf[OFF_BODY],
            node_index: buf[OFF_BODY + 1],
            node_count: buf[OFF_BODY + 2],
            flags: buf[OFF_BODY + 3],
            channels: ChannelSet::from_bytes(channels),
            tx_power: i8::from_le_bytes([buf[OFF_BODY + 4 + CHANNEL_SET_BYTES]]),
        })
    }

    /// Encode to the [`ADMIN_MSG_LEN`] bytes a wartui node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; ADMIN_MSG_LEN] {
        let mut out = [0u8; ADMIN_MSG_LEN];
        write_header(&mut out, MsgType::Admin);
        out[OFF_BODY] = self.epoch;
        out[OFF_BODY + 1] = self.node_index;
        out[OFF_BODY + 2] = self.node_count;
        out[OFF_BODY + 3] = self.flags;
        out[OFF_BODY + 4..OFF_BODY + 4 + CHANNEL_SET_BYTES]
            .copy_from_slice(&self.channels.to_bytes());
        out[OFF_BODY + 4 + CHANNEL_SET_BYTES] = self.tx_power.to_le_bytes()[0];
        out
    }
}

/// Core → node: forget every address it has reported.
///
/// Header only, no body — the whole instruction is the type byte. Idempotent,
/// so it carries no epoch: a node that already believes its ring is empty
/// clearing it again costs one extra round of sightings and nothing else, so
/// there is no state to disagree about the way an assignment's epoch guards
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearMsg;

impl ClearMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Clear => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() != CLEAR_MSG_LEN {
            return Err(DecodeError::BadLength { need: CLEAR_MSG_LEN, got: buf.len() });
        }
        Ok(Self)
    }

    /// Encode to the [`CLEAR_MSG_LEN`] bytes a wartui node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; CLEAR_MSG_LEN] {
        let mut out = [0u8; CLEAR_MSG_LEN];
        write_header(&mut out, MsgType::Clear);
        out
    }
}

/// Turn a host-side monotonic assignment counter into the byte the wire carries.
///
/// A node adopts an assignment only when this byte *differs* from the one it holds,
/// so an epoch that is re-used after a restart is silently discarded —
/// divergence 4. wartui persists a `u64` and narrows it here, skipping zero so a
/// node holding a freshly-zeroed field is not mistaken for one holding an
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
    Sightings(SightingBatch<'a>),
    /// Core → node. Seeing one this host did not send means another core is
    /// driving this fleet.
    Admin(AdminMsg),
    /// Core → node. Seeing one this host did not send means another core is
    /// driving this fleet, the same as [`Frame::Admin`].
    Clear(ClearMsg),
}

impl<'a> Frame<'a> {
    /// Decode any frame, choosing the layout from the type byte.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Heartbeat => HeartbeatMsg::decode(buf).map(Frame::Heartbeat),
            MsgType::SightingBatch => SightingBatch::decode(buf).map(Frame::Sightings),
            MsgType::Admin => AdminMsg::decode(buf).map(Frame::Admin),
            MsgType::Clear => ClearMsg::decode(buf).map(Frame::Clear),
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
/// The vendor magic is all that is matched. wartui's own frames carry [`MAGIC`],
/// so anything carrying `ENOW` belongs to somebody else by definition.
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
