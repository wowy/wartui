use core::fmt;

use super::{
    DecodeError, DieTemp, MsgType, NODE_VERSION, OFFSET_BODY, Vdd, Version, fixed, header, le_u32,
    write_header,
};
use crate::reset::ResetCause;

/// Length of [`HeartbeatMsg`] on the wire.
pub const HEARTBEAT_MSG_LEN: usize = OFFSET_BODY + 23;

/// Length of the frozen prefix that opens every heartbeat body: version, board, flags
/// ([`Capabilities::decode_prefix`]).
pub const HEARTBEAT_PREFIX_LEN: usize = 5;

/// [`Capabilities`] flags bit 0: this radio reaches 5 GHz.
pub const CAP_FLAG_5G: u8 = 1 << 0;
/// [`Capabilities`] flags bit 1: the node is running an image on trial, not yet confirmed.
pub const CAP_FLAG_TRIAL: u8 = 1 << 1;

/// The board a node firmware was built for, from its cargo features.
///
/// Each byte is fixed once assigned, since the heartbeat prefix that carries it is frozen.
/// 0 is never sent. A byte this build has no name for decodes to [`Board::Unknown`]: a node
/// on a board added later is still a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Board {
    /// An ESP32-C5 board: byte 1.
    Esp32c5,
    /// An ESP32-C6 board on its PCB antenna: byte 2.
    Esp32c6,
    /// A Seeed XIAO ESP32-C6 switched to its external antenna: byte 3.
    Esp32c6Xiao,
    /// An ESP32-P4 bridge: byte 4. No node runs on one; its update receiver names it. The P4
    /// fills a link update's `Begin` with its receiver's board, so the receiver's board check
    /// passes every image: `wartui flash-bridge` checks the image is the P4's before sending it.
    Esp32p4,
    /// A byte this build does not name.
    Unknown(u8),
}

impl Board {
    /// The byte the heartbeat carries.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Esp32c5 => 1,
            Self::Esp32c6 => 2,
            Self::Esp32c6Xiao => 3,
            Self::Esp32p4 => 4,
            Self::Unknown(byte) => byte,
        }
    }

    /// Read the heartbeat's board byte. Never fails.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Self {
        match byte {
            1 => Self::Esp32c5,
            2 => Self::Esp32c6,
            3 => Self::Esp32c6Xiao,
            4 => Self::Esp32p4,
            other => Self::Unknown(other),
        }
    }
}

/// What a node says it is, in every heartbeat it sends: the frozen prefix.
///
/// `five_ghz` is a refusal, not a request. A node without it is never dealt a 5 GHz
/// index, because a share it cannot tune is a share nobody scans. It travels as its own
/// flag rather than being read off `board`, so a board this build does not name still
/// says what its radio reaches. Bluetooth is not a capability: every node can scan it,
/// and [`ADMIN_FLAG_BLE`] is the operator's decision.
///
/// The prefix is the first [`HEARTBEAT_PREFIX_LEN`] bytes of the body and never changes
/// before 1.0 ([`Capabilities::decode_prefix`]).
///
/// [`ADMIN_FLAG_BLE`]: super::ADMIN_FLAG_BLE
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The firmware the node runs.
    pub version: Version,
    /// The board it was built for.
    pub board: Board,
    /// The radio reaches 5 GHz. False on an ESP32-C6, which is 2.4 GHz only.
    pub five_ghz: bool,
    /// The node runs an image it has not yet confirmed ([`CAP_FLAG_TRIAL`]).
    pub trial: bool,
}

impl Capabilities {
    /// What this build is on `board`, for a node to announce.
    #[must_use]
    pub const fn here(board: Board) -> Self {
        Self {
            version: NODE_VERSION,
            board,
            five_ghz: matches!(board, Board::Esp32c5),
            trial: false,
        }
    }

    /// The feature bits as the wire carries them.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        let mut flags = 0;
        if self.five_ghz {
            flags |= CAP_FLAG_5G;
        }
        if self.trial {
            flags |= CAP_FLAG_TRIAL;
        }
        flags
    }

    /// Rebuild from the five prefix bytes a heartbeat carries.
    ///
    /// Unknown flag bits are ignored, not refused. A node must not stop being a node
    /// because it sets a bit this build does not know.
    #[must_use]
    pub const fn from_prefix(prefix: [u8; HEARTBEAT_PREFIX_LEN]) -> Self {
        let [major, minor, patch, board, flags] = prefix;
        Self {
            version: Version::new(major, minor, patch),
            board: Board::from_u8(board),
            five_ghz: flags & CAP_FLAG_5G != 0,
            trial: flags & CAP_FLAG_TRIAL != 0,
        }
    }

    /// The five prefix bytes.
    #[must_use]
    pub const fn to_prefix(&self) -> [u8; HEARTBEAT_PREFIX_LEN] {
        [
            self.version.major,
            self.version.minor,
            self.version.patch,
            self.board.as_u8(),
            self.flags(),
        ]
    }

    /// Read the frozen prefix of any heartbeat, whatever build sent it.
    ///
    /// Accepts a frame with our magic, [`WIRE_VERSION`] and the heartbeat type whose body
    /// is at least [`HEARTBEAT_PREFIX_LEN`] bytes, at any total length. The rest of the
    /// body is a minor version's business and is not read.
    ///
    /// The frame header, the heartbeat type byte and this prefix never change before 1.0,
    /// the one exception to flashing the fleet together. A host finds a node a minor
    /// version behind through them, so it can say so and, over the air, update it.
    ///
    /// # Errors
    /// [`DecodeError::TooShort`] for a body shorter than the prefix, and whatever the
    /// header check refuses; any other type is [`DecodeError::UnknownType`].
    ///
    /// [`WIRE_VERSION`]: super::WIRE_VERSION
    pub fn decode_prefix(buf: &[u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::Heartbeat => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        let need = OFFSET_BODY + HEARTBEAT_PREFIX_LEN;
        let Some(prefix) = buf.get(OFFSET_BODY..need) else {
            return Err(DecodeError::TooShort { need, got: buf.len() });
        };
        Ok(Self::from_prefix([prefix[0], prefix[1], prefix[2], prefix[3], prefix[4]]))
    }
}

/// The form the fleet table, logs and the store column show: `0.1.0;5g`, with `;trial`
/// for an unconfirmed image. Only the prefix bytes travel; this is for reading.
impl fmt::Display for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.version)?;
        if self.five_ghz {
            f.write_str(";5g")?;
        }
        if self.trial {
            f.write_str(";trial")?;
        }
        Ok(())
    }
}

/// Node → host: a node's liveness, broadcast from the control channel.
///
/// Sent every [`IDLE_BEAT_MS`] while parked and every [`ASSIGNED_BEAT_MS`] while
/// assigned. Each one opens the node's admin window.
///
/// [`IDLE_BEAT_MS`]: crate::node::IDLE_BEAT_MS
/// [`ASSIGNED_BEAT_MS`]: crate::node::ASSIGNED_BEAT_MS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatMsg {
    /// Counts up from node boot: once per sweep, Bluetooth scan or idle beat. A value
    /// below the last one means the node restarted and has forgotten its assignment.
    pub counter: u32,
    /// The assignment epoch this node holds, or 0 for none (parked since boot). The
    /// host never puts 0 on the wire, so 0 is unambiguous.
    pub epoch: u8,
    /// What that node is.
    pub capabilities: Capabilities,
    /// Access points a full pending buffer turned away since boot, each once per dwell
    /// ([`crate::pending::Refused`]). Wraps. Most are reported on a later dwell, so this
    /// measures the buffer against the neighborhood rather than counting losses. The
    /// host reads it as a difference between heartbeats.
    pub wifi_refused: u16,
    /// Advertisers a full pending buffer turned away since boot, each once per scan.
    /// Wraps, and is read like `wifi_refused`. 0 on a node that has never held the
    /// Bluetooth scan.
    pub ble_refused: u16,
    /// Heartbeats this node has tried to send since boot, this one included, so the first
    /// carries 1. Wraps. A gap in `beat` is heartbeats lost between node and host.
    pub beat: u16,
    /// Heartbeats the radio could not send since boot, this one excluded: a heartbeat
    /// cannot know its own outcome. Wraps at 256, and is read as a difference between
    /// received heartbeats. A byte is enough. The failed sends in a gap cannot outnumber the
    /// beats in it, so the difference is exact for any gap under 256 beats.
    pub unsent: u8,
    /// The scan channel number (a [`SCAN_CHANNELS`] value, not an index) the node dwelt on
    /// just before this heartbeat, or 0 when no dwell preceded it: a parked node, the
    /// Bluetooth node, or a hop the radio refused. A sweep and a beat drift against each
    /// other, so loss that depends on the channel before the beat shows as a period in
    /// which beats go missing; this names that channel.
    ///
    /// [`SCAN_CHANNELS`]: crate::plan::SCAN_CHANNELS
    pub dwell: u8,
    /// The `dwell` the previous heartbeat carried, whether or not that heartbeat arrived,
    /// and 0 for the first heartbeat after boot. A single missed beat gets its channel from
    /// the next one that arrives.
    pub prev_dwell: u8,
    /// The radio's maximum transmit power in quarter-dBm, as `esp_wifi_get_max_tx_power`
    /// reports it just before this heartbeat, or 0 when the read failed. The IDF floor is
    /// 8, so 0 is unambiguous. Read on every beat rather than remembered from adoption, so
    /// a cap that a refused `set_max_tx_power` kept, or that something reset mid-boot,
    /// shows here. A refusal keeps whatever the radio already had: the 2 dBm boot default
    /// (8) if no earlier assignment set one, otherwise that assignment's power. [`crate::tx_power`] has the range the host configures.
    pub tx_power: i8,
    /// Why the node is running this boot. Read once at boot, so every heartbeat of a boot
    /// carries the same one.
    pub reset_cause: ResetCause,
    /// The chip's 3.3 V rail just before this heartbeat, or [`Vdd::NONE`] when the read
    /// failed.
    pub vdd: Vdd,
    /// The die temperature in whole degrees just before this heartbeat, offset by 40, or
    /// [`DieTemp::NONE`] when the read failed. The die runs well above the air around it.
    pub die_temp: DieTemp,
}

impl HeartbeatMsg {
    /// Decode from a received frame.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        fixed(buf, MsgType::Heartbeat, HEARTBEAT_MSG_LEN)?;
        const P: usize = OFFSET_BODY + HEARTBEAT_PREFIX_LEN;
        let capabilities = Capabilities::from_prefix([
            buf[OFFSET_BODY],
            buf[OFFSET_BODY + 1],
            buf[OFFSET_BODY + 2],
            buf[OFFSET_BODY + 3],
            buf[OFFSET_BODY + 4],
        ]);
        let le = |at: usize| u16::from_le_bytes([buf[at], buf[at + 1]]);
        Ok(Self {
            capabilities,
            counter: le_u32(buf, P),
            epoch: buf[P + 4],
            wifi_refused: le(P + 5),
            ble_refused: le(P + 7),
            beat: le(P + 9),
            unsent: buf[P + 11],
            dwell: buf[P + 12],
            prev_dwell: buf[P + 13],
            tx_power: i8::from_le_bytes([buf[P + 14]]),
            reset_cause: ResetCause::from_u8(buf[P + 15]),
            vdd: Vdd(buf[P + 16]),
            die_temp: DieTemp(buf[P + 17]),
        })
    }

    /// Encode to the twenty-nine bytes a heartbeat is.
    #[must_use]
    pub fn encode(&self) -> [u8; HEARTBEAT_MSG_LEN] {
        const P: usize = OFFSET_BODY + HEARTBEAT_PREFIX_LEN;
        let mut out = [0u8; HEARTBEAT_MSG_LEN];
        write_header(&mut out, MsgType::Heartbeat);
        out[OFFSET_BODY..P].copy_from_slice(&self.capabilities.to_prefix());
        out[P..P + 4].copy_from_slice(&self.counter.to_le_bytes());
        out[P + 4] = self.epoch;
        out[P + 5..P + 7].copy_from_slice(&self.wifi_refused.to_le_bytes());
        out[P + 7..P + 9].copy_from_slice(&self.ble_refused.to_le_bytes());
        out[P + 9..P + 11].copy_from_slice(&self.beat.to_le_bytes());
        out[P + 11] = self.unsent;
        out[P + 12] = self.dwell;
        out[P + 13] = self.prev_dwell;
        out[P + 14] = self.tx_power.to_le_bytes()[0];
        out[P + 15] = self.reset_cause.as_u8();
        out[P + 16] = self.vdd.0;
        out[P + 17] = self.die_temp.0;
        out
    }
}
