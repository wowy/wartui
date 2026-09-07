//! The on-air ESP-NOW frames.
//!
//! Little-endian, no CRC, no protocol version field. See `src/WiFiOps.h:66-82`.
//! Encoding and decoding are written out by hand rather than transmuting a
//! `#[repr(packed)]` struct: the byte layout is a contract with a separately
//! compiled program, so it deserves to be spelled out and tested against real
//! bytes.
//!
//! Node → core is still the vendor's format exactly, and is meant to stay that
//! way: a heartbeat and an observation from a wartui node are byte-identical to
//! a stock one's, which is what lets the golden vectors captured off a vendor
//! fleet keep testing this code.
//!
//! Core → node is not, from Phase 2. [`AdminMsg`] is wartui's own fourteen-byte
//! frame and the vendor's ten-byte one no longer decodes: a channel *set*
//! replaces the pair of bounds, and a flags byte says whether the node should
//! scan Bluetooth. Nothing is done to keep a mixed fleet working, because a
//! mixed fleet was never the point — the vendor node is the thing being
//! replaced. What survives is [`is_legacy_admin`], which recognises the old
//! shape without decoding it, so a stock core powered up nearby is reported as
//! the operational hazard it is rather than counted as line noise.

use core::fmt;

use crate::plan::{CHANNEL_SET_BYTES, ChannelSet};

/// Frame preamble, `"ENOW"`. Four raw bytes, not a NUL-terminated string.
/// `src/WiFiOps.cpp:13`.
pub const MAGIC: [u8; 4] = *b"ENOW";

/// Largest text payload a node will emit. `src/configs.h:53`.
pub const ENOW_TEXT_MAX: usize = 200;

/// `sizeof(enow_text_msg_t)`. Every send transmits the whole struct regardless
/// of how much text it carries, and every receive handler drops anything
/// shorter, so encoders must pad to exactly this. `src/WiFiOps.cpp:1004`.
pub const TEXT_MSG_LEN: usize = 212;

/// Length of wartui's [`AdminMsg`] on the wire.
///
/// Four bytes of magic, the type byte, version, index, count, flags, and five
/// bytes of channel mask.
pub const ADMIN_MSG_LEN: usize = 9 + CHANNEL_SET_BYTES;

/// `sizeof(enow_admin_msg_t)`, the vendor core's assignment
/// (`src/WiFiOps.cpp:1194`). Nothing decodes it any more; it is here so
/// [`is_legacy_admin`] can name the shape it is looking for.
pub const LEGACY_ADMIN_MSG_LEN: usize = 10;

/// [`AdminMsg::flags`] bit 0: scan Bluetooth as well as Wi-Fi.
///
/// Off is the safe default and the one a node boots into whatever its build
/// says, because the coexistence cost is real and measured
/// (`docs/phase-0-findings.md`): on a stock node BLE cost every one of the
/// thirty-two assignments sent to it. The `ble` cargo feature decides only
/// whether the code is compiled in; this bit decides whether it runs.
pub const ADMIN_FLAG_BLE: u8 = 1 << 0;

/// Buffer size [`WardriveLine::write_into`] can always finish in.
///
/// 17 for the BSSID, 32 for the longest SSID 802.11 allows, 15 for
/// `[WPA2_WPA3_PSK]`, 5 for a `u16` channel, 6 for an `i16` RSSI, one for the
/// kind and five separating commas. Comfortably inside [`ENOW_TEXT_MAX`], which
/// is what actually bounds the frame.
///
/// The channel and RSSI widths are the parsed ones, not the node's. A node
/// encodes a `u8` channel and an `i8` RSSI and cannot reach 77 bytes; a host
/// re-encoding a line it read off the wire can, and a buffer sized for the node
/// would drop those records silently rather than truncate them.
pub const WARDRIVE_LINE_MAX: usize = 81;

const OFF_TYPE: usize = 4;
const OFF_COUNTER: usize = 5;
const OFF_LEN: usize = 9;
const OFF_TEXT: usize = 11;

const OFF_VERSION: usize = 5;
const OFF_NODE_INDEX: usize = 6;
const OFF_NODE_COUNT: usize = 7;
const OFF_FLAGS: usize = 8;
const OFF_CHANNELS: usize = 9;

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

/// What a node says it is, in the text field of every heartbeat it sends.
///
/// Node → core is byte-identical to a stock node's, which is what lets vendor
/// golden vectors keep testing this code — and it is also why the host cannot
/// otherwise tell one firmware from the other. A stock node heartbeats like
/// ours, is planned for like ours, and its radio acknowledges an assignment its
/// application cannot decode; it then keeps scanning all forty channels while
/// the share cut for it goes uncovered. Measured on a bench in
/// `docs/phase-2-findings.md`: two nodes and a stranger covered less of the
/// pool than the two nodes would have covered alone.
///
/// The heartbeat's text field is the place for the answer because it costs
/// nothing. It is already on the wire, a stock node leaves it empty
/// (`src/WiFiOps.cpp:1456-1457`), and every heartbeat carries it — so the host
/// learns what a node is from the first frame it could act on, before it has
/// to choose anything, and relearns it if the node is reflashed with something
/// else. No handshake, no request, nothing to lose.
///
/// The token is ASCII, `wartui/<major>.<minor>` followed by an optional `;` and
/// a comma-separated feature list: `wartui/0.1;ble,5g`. Unknown features are
/// ignored rather than refused, because a node from a later build must not stop
/// being a node just because it can do something new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Bumped when node → core changes shape. Nothing gates on it yet — no such
    /// change has happened — but it is the lever available when one does, and a
    /// version that is on the wire from the start is one that need not be
    /// retrofitted to a fleet already in the field.
    pub major: u8,
    /// Bumped for additions a older host can ignore, such as a new feature
    /// token.
    pub minor: u8,
    /// Built with the `ble` cargo feature, so the Bluetooth scan is code this
    /// node actually has. Says nothing about whether it is running: that is the
    /// assignment's [`ADMIN_FLAG_BLE`], and it is off at every boot.
    pub ble: bool,
    /// The radio reaches 5 GHz. False on an ESP32-C6, which is 2.4 GHz only, so
    /// a share of 5 GHz channels dealt to one is a share nobody scans.
    pub five_ghz: bool,
}

/// The node protocol version this build speaks, written into every heartbeat.
pub const CAPABILITY_MAJOR: u8 = 0;
/// See [`CAPABILITY_MAJOR`].
pub const CAPABILITY_MINOR: u8 = 1;

/// Longest token [`Capabilities::write_into`] can produce, for sizing a buffer.
///
/// `wartui/255.255;ble,5g` is 21; the room above it is for one more feature
/// name without every caller having to be found again.
pub const CAPABILITY_MAX: usize = 32;

const CAPABILITY_PREFIX: &[u8] = b"wartui/";

impl Capabilities {
    /// What this build is, for a node to announce.
    #[must_use]
    pub const fn here(ble: bool, five_ghz: bool) -> Self {
        Self { major: CAPABILITY_MAJOR, minor: CAPABILITY_MINOR, ble, five_ghz }
    }

    /// Read the token out of a heartbeat's text field.
    ///
    /// `None` for anything that is not one, which is the ordinary case for a
    /// stock node and for any wartui node built before this existed. It is a
    /// deliberate silence rather than an error: nothing is wrong with such a
    /// node, the host simply cannot use it.
    #[must_use]
    pub fn parse(text: &[u8]) -> Option<Self> {
        let rest = text.strip_prefix(CAPABILITY_PREFIX)?;
        // Split the version from the features first, so a malformed feature
        // list cannot make a well-formed version unreadable.
        let (version, features) = match rest.iter().position(|b| *b == b';') {
            Some(at) => (&rest[..at], &rest[at + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        let dot = version.iter().position(|b| *b == b'.')?;
        let major = ascii_u8(&version[..dot])?;
        let minor = ascii_u8(&version[dot + 1..])?;

        let mut out = Self { major, minor, ble: false, five_ghz: false };
        for feature in features.split(|b| *b == b',') {
            match feature {
                b"ble" => out.ble = true,
                b"5g" => out.five_ghz = true,
                // Something a later build knows about. Carrying on is the whole
                // point of a feature list: a node is not disqualified by having
                // grown a capability this host has never heard of.
                _ => {}
            }
        }
        Some(out)
    }

    /// Write the token into `out`, returning how many bytes it used.
    ///
    /// `None` if `out` is shorter than the token needs; [`CAPABILITY_MAX`] is
    /// always enough.
    pub fn write_into(&self, out: &mut [u8]) -> Option<usize> {
        let mut at = 0usize;
        let mut push = |bytes: &[u8], at: &mut usize| -> Option<()> {
            out.get_mut(*at..*at + bytes.len())?.copy_from_slice(bytes);
            *at += bytes.len();
            Some(())
        };
        push(CAPABILITY_PREFIX, &mut at)?;
        let mut digits = [0u8; 3];
        push(u8_ascii(self.major, &mut digits), &mut at)?;
        push(b".", &mut at)?;
        let mut digits = [0u8; 3];
        push(u8_ascii(self.minor, &mut digits), &mut at)?;

        let mut first = true;
        for (present, name) in [(self.ble, &b"ble"[..]), (self.five_ghz, &b"5g"[..])] {
            if !present {
                continue;
            }
            push(if first { b";" } else { b"," }, &mut at)?;
            push(name, &mut at)?;
            first = false;
        }
        Some(at)
    }
}

/// Parse ASCII decimal digits, rejecting anything else. `no_std`, so this is
/// written out rather than reached for.
fn ascii_u8(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() || bytes.len() > 3 {
        return None;
    }
    let mut value: u16 = 0;
    for b in bytes {
        let digit = b.checked_sub(b'0').filter(|d| *d <= 9)?;
        value = value * 10 + u16::from(digit);
    }
    u8::try_from(value).ok()
}

/// The other direction, into a caller-owned buffer.
fn u8_ascii(value: u8, out: &mut [u8; 3]) -> &[u8] {
    let mut at = out.len();
    let mut left = value;
    loop {
        at -= 1;
        out[at] = b'0' + left % 10;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    &out[at..]
}

/// wartui's channel assignment — the one frame a node accepts.
///
/// Fourteen bytes: magic, type 5, `assignment_version`, `node_index`,
/// `node_count`, [`flags`](Self::flags), then five bytes of [`ChannelSet`].
///
/// It replaced the vendor's ten-byte `enow_admin_msg_t` in Phase 2 and is not
/// compatible with it; [`is_legacy_admin`] is all that remains of that shape.
/// The pair of `SCAN_CHANNELS` bounds became a forty-bit mask because a run
/// cannot describe a restricted pool: the US pool is 2.4 GHz 1-11 and 5 GHz
/// 36-165 with a gap between, so a node holding it needed two assignments in
/// sequence, on a dwell timer, with coverage intermittent in between. A mask
/// says it in one frame, and lets the planner deal channels round-robin so
/// every node carries some of both bands.
///
/// Everything below is why the *delivery* of this frame is shaped the way it
/// is, and none of it changed with the layout.
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
    /// from the one it currently holds (`!=`, not `>`, `src/WiFiOps.cpp:1198`),
    /// and never uses 0 on the wire.
    pub assignment_version: u8,
    /// This node's slot in the fleet-wide staggering order.
    pub node_index: u8,
    /// Fleet size the stagger is computed against.
    pub node_count: u8,
    /// Per-node switches. [`ADMIN_FLAG_BLE`] is the only one defined.
    ///
    /// Carried whole rather than unpacked into `bool`s so an unknown bit set by
    /// a newer host survives a decode and re-encode instead of being quietly
    /// dropped — the same reason [`Security::Other`] exists.
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
        let mut channels = [0u8; CHANNEL_SET_BYTES];
        channels.copy_from_slice(&buf[OFF_CHANNELS..OFF_CHANNELS + CHANNEL_SET_BYTES]);
        Ok(Self {
            assignment_version: buf[OFF_VERSION],
            node_index: buf[OFF_NODE_INDEX],
            node_count: buf[OFF_NODE_COUNT],
            flags: buf[OFF_FLAGS],
            channels: ChannelSet::from_bytes(channels),
        })
    }

    /// Encode to the 14 bytes a wartui node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; ADMIN_MSG_LEN] {
        let mut out = [0u8; ADMIN_MSG_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[OFF_TYPE] = MsgType::Admin.as_u8();
        out[OFF_VERSION] = self.assignment_version;
        out[OFF_NODE_INDEX] = self.node_index;
        out[OFF_NODE_COUNT] = self.node_count;
        out[OFF_FLAGS] = self.flags;
        out[OFF_CHANNELS..].copy_from_slice(&self.channels.to_bytes());
        out
    }
}

/// Whether `buf` is the vendor core's ten-byte assignment.
///
/// Nothing here decodes one — the fields are not ours any more and acting on
/// them would be adopting another core's idea of the fleet. It is recognised
/// because it has to be *reported*: a stock core powered up in the same room
/// is assigning wartui's nodes channels wartui did not choose, and the symptom
/// is a fleet that keeps changing its mind for no reason this host can see.
/// Counting it as line noise would hide the one clue.
///
/// Deliberately exact on length. ESP-NOW delivers a frame at the length it was
/// sent, so a ten-byte type-5 frame is the vendor's and a fourteen-byte one is
/// [`AdminMsg`]; nothing has to guess.
#[must_use]
pub fn is_legacy_admin(buf: &[u8]) -> bool {
    buf.len() == LEGACY_ADMIN_MSG_LEN
        && buf[..4] == MAGIC
        && buf[OFF_TYPE] == MsgType::Admin.as_u8()
}

/// Turn a host-side monotonic assignment counter into the byte the wire carries.
///
/// Divergence 4. The vendor core keeps `current_assignment_version` in RAM and
/// resets it to 1 at every boot (`src/WiFiOps.h:218`), while nodes adopt an
/// assignment only when the byte *differs* from the one they hold (`!=`, not
/// `>`, `src/WiFiOps.cpp:1198`). Between them those two facts mean a core that
/// restarts and recomputes the same assignment is silently ignored by every
/// node that already holds it — and if the topology changed while the core was
/// down, the two views diverge permanently with nothing to say so.
///
/// wartui persists a `u64` instead and narrows it here. Zero is skipped
/// because the firmware never puts it on the wire, so a node holding a
/// freshly-zeroed field cannot be mistaken for one holding an assignment.
#[must_use]
pub const fn wire_version(counter: u64) -> u8 {
    // `% 255` lands in 0..=254; the offset moves that to 1..=255.
    ((counter.wrapping_sub(1) % 255) as u8) + 1
}

/// Either kind of frame, dispatched on the type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A 212-byte text-shaped frame.
    Text(TextMsg<'a>),
    /// A 14-byte assignment. The vendor's ten-byte one is not one of these;
    /// see [`is_legacy_admin`].
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

    /// Write the payload a node transmits, returning how many bytes it took.
    ///
    /// The inverse of [`Self::parse`], and the half the host never needed:
    /// wartui only ever read these lines until a node of our own had to emit
    /// them. [`WARDRIVE_LINE_MAX`] is a buffer this can always finish in;
    /// anything smaller may return `None`, and nothing is written when it does.
    ///
    /// Two details are the firmware's rather than ours. Commas inside the SSID
    /// become underscores, because the core splits on `,` and counts six fields
    /// (`ssid.replace(",","_")`, `src/WiFiOps.cpp:1768`) — an SSID containing
    /// one would otherwise take the record apart. And the BSSID is uppercase
    /// hex for Wi-Fi and lowercase for BLE, which is not a choice so much as
    /// the two paths having been written by different hands
    /// (`WiFi.BSSIDstr()` at `src/WiFiOps.cpp:1777` against NimBLE's
    /// `toString()` at `:144`); [`Self::parse`] normalises it away again.
    #[must_use]
    pub fn write_into(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer { out, at: 0 };
        w.mac(&self.bssid, self.kind == RecordKind::Wifi)?;
        w.byte(b',')?;
        for &b in self.ssid {
            w.byte(if b == b',' { b'_' } else { b })?;
        }
        w.byte(b',')?;
        w.bytes(self.security.as_bytes())?;
        w.byte(b',')?;
        w.u16(self.channel)?;
        w.byte(b',')?;
        w.i16(self.rssi)?;
        w.byte(b',')?;
        w.bytes(match self.kind {
            RecordKind::Wifi => b"W",
            RecordKind::Ble => b"B",
        })?;
        Some(w.at)
    }
}

/// A cursor over the caller's buffer, so a line that does not fit stops at the
/// first byte that would not rather than being written half-formed.
struct Writer<'b> {
    out: &'b mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn byte(&mut self, b: u8) -> Option<()> {
        *self.out.get_mut(self.at)? = b;
        self.at += 1;
        Some(())
    }

    fn bytes(&mut self, bytes: &[u8]) -> Option<()> {
        for &b in bytes {
            self.byte(b)?;
        }
        Some(())
    }

    fn mac(&mut self, mac: &[u8; 6], upper: bool) -> Option<()> {
        const UPPER: &[u8; 16] = b"0123456789ABCDEF";
        const LOWER: &[u8; 16] = b"0123456789abcdef";
        let digits = if upper { UPPER } else { LOWER };
        for (i, &octet) in mac.iter().enumerate() {
            if i > 0 {
                self.byte(b':')?;
            }
            self.byte(digits[usize::from(octet >> 4)])?;
            self.byte(digits[usize::from(octet & 0x0F)])?;
        }
        Some(())
    }

    fn u16(&mut self, mut value: u16) -> Option<()> {
        let mut digits = [0u8; 5];
        let mut n = 0;
        loop {
            // Cast is safe: a decimal digit is 0..=9.
            #[allow(clippy::cast_possible_truncation)]
            {
                digits[n] = b'0' + (value % 10) as u8;
            }
            n += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for &d in digits[..n].iter().rev() {
            self.byte(d)?;
        }
        Some(())
    }

    fn i16(&mut self, value: i16) -> Option<()> {
        if value < 0 {
            self.byte(b'-')?;
        }
        // Through `u16` rather than `-value`, so `i16::MIN` is not a panic
        // waiting for a receiver with an implausible reading.
        self.u16(value.unsigned_abs())
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
