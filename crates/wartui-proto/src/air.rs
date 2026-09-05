//! The on-air ESP-NOW frames.
//!
//! Two packed little-endian structs, no CRC, no protocol version field. See
//! `src/WiFiOps.h:66-82`. Encoding and decoding are written out by hand rather
//! than transmuting a `#[repr(packed)]` struct: the byte layout is a contract
//! with a separately-compiled C++ program, so it deserves to be spelled out and
//! tested against real bytes.

use core::fmt;

/// Frame preamble, `"ENOW"`. Four raw bytes, not a NUL-terminated string.
/// `src/WiFiOps.cpp:13`.
pub const MAGIC: [u8; 4] = *b"ENOW";

/// Largest text payload a node will emit. `src/configs.h:53`.
pub const ENOW_TEXT_MAX: usize = 200;

/// `sizeof(enow_text_msg_t)`. Every send transmits the whole struct regardless
/// of how much text it carries, and every receive handler drops anything
/// shorter, so encoders must pad to exactly this. `src/WiFiOps.cpp:1004`.
pub const TEXT_MSG_LEN: usize = 212;

/// `sizeof(enow_admin_msg_t)`. `src/WiFiOps.cpp:1194`.
pub const ADMIN_MSG_LEN: usize = 10;

const OFF_TYPE: usize = 4;
const OFF_COUNTER: usize = 5;
const OFF_LEN: usize = 9;
const OFF_TEXT: usize = 11;

/// `enum MsgType : uint8_t`, `src/WiFiOps.cpp:88-94`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgType {
    /// Node → core. Broadcast, encrypted sessions only. wartui runs plaintext,
    /// so receiving one means that node has encryption switched on.
    CoreRequest = 1,
    /// Core → node, plaintext unicast, so the node learns the core's MAC.
    CoreReply = 2,
    /// Node → core, once per completed channel sweep. `counter` is monotonic
    /// from node boot.
    Heartbeat = 3,
    /// Node → core, one per newly-seen BSSID. Carries a [`WardriveLine`].
    Text = 4,
    /// Core → node. The only command a node accepts.
    Admin = 5,
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
            1 => Ok(Self::CoreRequest),
            2 => Ok(Self::CoreReply),
            3 => Ok(Self::Heartbeat),
            4 => Ok(Self::Text),
            5 => Ok(Self::Admin),
            other => Err(DecodeError::UnknownType(other)),
        }
    }
}

/// Why a byte slice was not a valid frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the message type requires. The firmware drops these
    /// silently; we report them so the bridge can count truncation.
    TooShort {
        /// Bytes required for this message type.
        need: usize,
        /// Bytes actually present.
        got: usize,
    },
    /// First four bytes were not `"ENOW"`.
    BadMagic,
    /// Type byte outside 1..=5.
    UnknownType(u8),
    /// `len` exceeded [`ENOW_TEXT_MAX`]. The firmware logs and drops these
    /// (`src/WiFiOps.cpp:1149`).
    TextTooLong(u16),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { need, got } => write!(f, "frame too short: need {need}, got {got}"),
            Self::BadMagic => f.write_str("bad magic, expected \"ENOW\""),
            Self::UnknownType(t) => write!(f, "unknown message type {t}"),
            Self::TextTooLong(n) => write!(f, "text length {n} exceeds {ENOW_TEXT_MAX}"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// A decoded `enow_text_msg_t`, borrowing its payload from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextMsg<'a> {
    /// One of `CoreRequest`, `CoreReply`, `Heartbeat` or `Text`.
    pub msg_type: MsgType,
    /// Heartbeat counter, monotonic from node boot. Meaningful only for
    /// [`MsgType::Heartbeat`]; a regression means the node rebooted.
    pub counter: u32,
    /// The `len` bytes of payload, without the trailing NUL or the zero padding.
    pub text: &'a [u8],
}

impl<'a> TextMsg<'a> {
    /// A text frame carrying `text`.
    ///
    /// # Errors
    /// [`DecodeError::TextTooLong`] if `text` exceeds [`ENOW_TEXT_MAX`].
    pub fn new(msg_type: MsgType, counter: u32, text: &'a [u8]) -> Result<Self, DecodeError> {
        if text.len() > ENOW_TEXT_MAX {
            // Cast is safe: the length is at most ENOW_TEXT_MAX + 1 here in
            // practice, and any larger value still reports usefully.
            return Err(DecodeError::TextTooLong(u16::try_from(text.len()).unwrap_or(u16::MAX)));
        }
        Ok(Self { msg_type, counter, text })
    }

    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        if buf.len() < TEXT_MSG_LEN {
            return Err(DecodeError::TooShort { need: TEXT_MSG_LEN, got: buf.len() });
        }
        if buf[..4] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let msg_type = MsgType::try_from(buf[OFF_TYPE])?;
        let counter = u32::from_le_bytes([
            buf[OFF_COUNTER],
            buf[OFF_COUNTER + 1],
            buf[OFF_COUNTER + 2],
            buf[OFF_COUNTER + 3],
        ]);
        let len = u16::from_le_bytes([buf[OFF_LEN], buf[OFF_LEN + 1]]);
        if usize::from(len) > ENOW_TEXT_MAX {
            return Err(DecodeError::TextTooLong(len));
        }
        Ok(Self { msg_type, counter, text: &buf[OFF_TEXT..OFF_TEXT + usize::from(len)] })
    }

    /// Encode to the full padded 212 bytes the firmware expects.
    #[must_use]
    pub fn encode(&self) -> [u8; TEXT_MSG_LEN] {
        let mut out = [0u8; TEXT_MSG_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[OFF_TYPE] = self.msg_type.as_u8();
        out[OFF_COUNTER..OFF_COUNTER + 4].copy_from_slice(&self.counter.to_le_bytes());
        // `new` and `decode` both bound this, so the cast cannot lose data.
        let len = self.text.len().min(ENOW_TEXT_MAX);
        #[allow(clippy::cast_possible_truncation)]
        out[OFF_LEN..OFF_LEN + 2].copy_from_slice(&(len as u16).to_le_bytes());
        out[OFF_TEXT..OFF_TEXT + len].copy_from_slice(&self.text[..len]);
        out
    }
}

/// A decoded `enow_admin_msg_t` — a channel-range assignment.
///
/// Observed on real hardware: a vendor core's assignment to its single node
/// went out once and was retransmitted 31 times by the radio, all 32 frames
/// sharing one 802.11 sequence number with the retry bit set on all but the
/// first. Nothing acknowledged it, yet the core cleared its dirty flag from the
/// `esp_now_send` return value (`src/WiFiOps.cpp:679`) and moved on believing
/// the node had been assigned. Broadcast heartbeats and observations from the
/// same node in the same capture each carried their own sequence number and
/// were never retried, so the retries are specific to unicast. Acknowledgements
/// were then captured directly: of 9505 seen on the channel, none named the
/// core, so the node genuinely never answered.
///
/// This is why wartui clears an assignment only on the transmit callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminMsg {
    /// Epoch counter. A node adopts the assignment only when this *differs*
    /// from the one it currently holds (`!=`, not `>`, `src/WiFiOps.cpp:1198`),
    /// and never uses 0 on the wire.
    pub assignment_version: u8,
    /// This node's slot in the fleet-wide staggering order.
    pub node_index: u8,
    /// Fleet size the stagger is computed against.
    pub node_count: u8,
    /// First index into [`SCAN_CHANNELS`](crate::plan::SCAN_CHANNELS), inclusive.
    pub start_channel_idx: u8,
    /// Last index into [`SCAN_CHANNELS`](crate::plan::SCAN_CHANNELS), inclusive.
    pub end_channel_idx: u8,
}

impl AdminMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        if buf.len() < ADMIN_MSG_LEN {
            return Err(DecodeError::TooShort { need: ADMIN_MSG_LEN, got: buf.len() });
        }
        if buf[..4] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        match MsgType::try_from(buf[OFF_TYPE])? {
            MsgType::Admin => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        Ok(Self {
            assignment_version: buf[5],
            node_index: buf[6],
            node_count: buf[7],
            start_channel_idx: buf[8],
            end_channel_idx: buf[9],
        })
    }

    /// Encode to the 10 bytes the firmware expects.
    #[must_use]
    pub fn encode(&self) -> [u8; ADMIN_MSG_LEN] {
        let mut out = [0u8; ADMIN_MSG_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[OFF_TYPE] = MsgType::Admin.as_u8();
        out[5] = self.assignment_version;
        out[6] = self.node_index;
        out[7] = self.node_count;
        out[8] = self.start_channel_idx;
        out[9] = self.end_channel_idx;
        out
    }
}

/// Either kind of frame, dispatched on the type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A 212-byte text-shaped frame.
    Text(TextMsg<'a>),
    /// A 10-byte assignment.
    Admin(AdminMsg),
}

impl<'a> Frame<'a> {
    /// Decode any frame, choosing the layout from the type byte.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        // Magic plus the type byte is the least that lets us pick a layout;
        // the firmware applies the same floor at `src/WiFiOps.cpp:990`.
        if buf.len() < 5 {
            return Err(DecodeError::TooShort { need: 5, got: buf.len() });
        }
        if buf[..4] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        match MsgType::try_from(buf[OFF_TYPE])? {
            MsgType::Admin => AdminMsg::decode(buf).map(Frame::Admin),
            _ => TextMsg::decode(buf).map(Frame::Text),
        }
    }
}

/// What a [`WardriveLine`] observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// A Wi-Fi access point. Trailing field `W`.
    Wifi,
    /// A BLE advertiser. Trailing field `B`.
    Ble,
}

/// The `AuthMode` token, as produced by `security_int_to_string`
/// (`src/WiFiOps.cpp:1833-1878`).
///
/// The firmware maps only nine `wifi_auth_mode_t` values and collapses
/// everything else — WPA3-Enterprise, OWE, WPA3-192 — into `[UNDEFINED]`. The
/// set is open-ended, so an unrecognised token is carried through as
/// [`Security::Other`] rather than failing the parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Security<'a> {
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
    /// `[BLE]`, the placeholder the BLE path emits.
    Ble,
    /// A token this firmware version did not have when wartui was written.
    Other(&'a [u8]),
}

impl<'a> Security<'a> {
    fn parse(raw: &'a [u8]) -> Self {
        match raw {
            b"[OPEN]" => Self::Open,
            b"[WEP]" => Self::Wep,
            b"[WPA_PSK]" => Self::WpaPsk,
            b"[WPA2_PSK]" => Self::Wpa2Psk,
            b"[WPA_WPA2_PSK]" => Self::WpaWpa2Psk,
            b"[WPA2]" => Self::Wpa2Enterprise,
            b"[WPA3_PSK]" => Self::Wpa3Psk,
            b"[WPA2_WPA3_PSK]" => Self::Wpa2Wpa3Psk,
            b"[WAPI_PSK]" => Self::WapiPsk,
            b"[UNDEFINED]" => Self::Undefined,
            b"[BLE]" => Self::Ble,
            other => Self::Other(other),
        }
    }

    /// The token exactly as it appeared on the wire, for round-tripping into
    /// the WiGLE `AuthMode` column.
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        match self {
            Self::Open => b"[OPEN]",
            Self::Wep => b"[WEP]",
            Self::WpaPsk => b"[WPA_PSK]",
            Self::Wpa2Psk => b"[WPA2_PSK]",
            Self::WpaWpa2Psk => b"[WPA_WPA2_PSK]",
            Self::Wpa2Enterprise => b"[WPA2]",
            Self::Wpa3Psk => b"[WPA3_PSK]",
            Self::Wpa2Wpa3Psk => b"[WPA2_WPA3_PSK]",
            Self::WapiPsk => b"[WAPI_PSK]",
            Self::Undefined => b"[UNDEFINED]",
            Self::Ble => b"[BLE]",
            Self::Other(raw) => raw,
        }
    }
}

/// Why a [`MsgType::Text`] payload was not a valid wardrive line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    /// Not exactly six comma-separated fields. `parseWardriveLine` applies the
    /// same rule (`src/WiFiOps.cpp:952-981`).
    FieldCount(usize),
    /// The BSSID was not 17 characters of colon-separated hex.
    BadBssid,
    /// The channel field was not a number.
    BadChannel,
    /// The RSSI field was not a number.
    BadRssi,
    /// The trailing field was neither `W` nor `B`.
    BadKind,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldCount(n) => write!(f, "expected 6 comma-separated fields, got {n}"),
            Self::BadBssid => f.write_str("malformed BSSID"),
            Self::BadChannel => f.write_str("malformed channel"),
            Self::BadRssi => f.write_str("malformed RSSI"),
            Self::BadKind => f.write_str("record kind was neither W nor B"),
        }
    }
}

impl core::error::Error for LineError {}

/// One observation, parsed from a [`MsgType::Text`] payload.
///
/// The payload is `bssid,essid,security,channel,rssi,type`
/// (`src/WiFiOps.cpp:1777` and `:144`). It is deliberately parsed from bytes
/// rather than `str`: the SSID is whatever the access point beaconed, so it may
/// be invalid UTF-8. The firmware replaces commas in it with underscores
/// (`ssid.replace(",","_")`), which is what makes splitting on `,` safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WardriveLine<'a> {
    /// Six raw bytes. Normalised here because the Wi-Fi path emits uppercase
    /// hex and the BLE path lowercase, and treating those as different keys
    /// would double-count.
    pub bssid: [u8; 6],
    /// Raw SSID bytes, possibly empty (always empty for BLE) and possibly not
    /// UTF-8.
    pub ssid: &'a [u8],
    /// The `AuthMode` token.
    pub security: Security<'a>,
    /// Wi-Fi channel number, or 0 for BLE.
    pub channel: u16,
    /// Signal strength in dBm, as the node measured it.
    pub rssi: i16,
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
}

impl<'a> WardriveLine<'a> {
    /// Parse a text payload.
    ///
    /// # Errors
    /// See [`LineError`].
    pub fn parse(raw: &'a [u8]) -> Result<Self, LineError> {
        let mut fields = [&[][..]; 6];
        let mut count = 0usize;
        for field in raw.split(|&b| b == b',') {
            if count < fields.len() {
                fields[count] = field;
            }
            count += 1;
        }
        if count != 6 {
            return Err(LineError::FieldCount(count));
        }

        let kind = match fields[5] {
            b"W" => RecordKind::Wifi,
            b"B" => RecordKind::Ble,
            _ => return Err(LineError::BadKind),
        };

        Ok(Self {
            bssid: parse_mac(fields[0]).ok_or(LineError::BadBssid)?,
            ssid: fields[1],
            security: Security::parse(fields[2]),
            channel: parse_u16(fields[3]).ok_or(LineError::BadChannel)?,
            rssi: parse_i16(fields[4]).ok_or(LineError::BadRssi)?,
            kind,
        })
    }
}

/// `AA:BB:CC:DD:EE:FF` in either case to six bytes.
fn parse_mac(raw: &[u8]) -> Option<[u8; 6]> {
    if raw.len() != 17 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        let at = i * 3;
        if i > 0 && raw[at - 1] != b':' {
            return None;
        }
        *byte = (hex_nibble(raw[at])? << 4) | hex_nibble(raw[at + 1])?;
    }
    Some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_u16(raw: &[u8]) -> Option<u16> {
    if raw.is_empty() {
        return None;
    }
    let mut acc: u16 = 0;
    for &c in raw {
        let d = c.checked_sub(b'0').filter(|d| *d <= 9)?;
        acc = acc.checked_mul(10)?.checked_add(u16::from(d))?;
    }
    Some(acc)
}

fn parse_i16(raw: &[u8]) -> Option<i16> {
    let (negative, digits) = match raw.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, raw),
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: i16 = 0;
    for &c in digits {
        let d = c.checked_sub(b'0').filter(|d| *d <= 9)?;
        acc = acc.checked_mul(10)?.checked_sub(i16::from(d))?;
    }
    if negative { Some(acc) } else { acc.checked_neg() }
}
