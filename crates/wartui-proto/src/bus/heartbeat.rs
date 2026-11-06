use core::fmt;

use super::msg::{fixed, le_u32};
use super::{DecodeError, DieTemp, NODE_VERSION, Vdd, Version};
use crate::mac::Mac;
use crate::reset::ResetCause;

/// Length of a [`Heartbeat`] body.
pub const HEARTBEAT_LEN: usize = 23;

/// Length of the prefix and MAC that open every heartbeat body ([`Heartbeat::identity`]).
pub const HEARTBEAT_IDENTITY_LEN: usize = HEARTBEAT_PREFIX_LEN + 6;

/// Length of the prefix that opens every heartbeat body: version, board, flags
/// ([`Capabilities::decode_prefix`]).
pub const HEARTBEAT_PREFIX_LEN: usize = 5;

/// [`Capabilities`] flags bit 0: this radio reaches 5 GHz.
pub const CAP_FLAG_5G: u8 = 1 << 0;
/// [`Capabilities`] flags bit 1: the node is running an image on trial, not yet confirmed.
pub const CAP_FLAG_TRIAL: u8 = 1 << 1;

/// The board a firmware image was built for.
///
/// Each byte is fixed once assigned. 0 is never sent, and 3 stays unassigned. A byte this build has no name for decodes to
/// [`Board::Unknown`]: a node on a board added later is still a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Board {
    /// An ESP32-C5 board: byte 1.
    Esp32c5,
    /// The ESP32-C6 that runs a P4 bridge's radio: byte 2. No node runs on one; its update
    /// receiver names it.
    Esp32c6,
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
            4 => Self::Esp32p4,
            other => Self::Unknown(other),
        }
    }
}

/// What a node says it is, in every heartbeat it sends: the prefix.
///
/// `five_ghz` is shown to the operator, and the planner does not read it: every node that speaks
/// this build's version is a C5. It travels as its own flag rather than being read off `board`, so
/// a board this build does not name still says what its radio reaches. Bluetooth is not a
/// capability: every node can scan it, and [`ASSIGN_FLAG_BLE`] is the operator's decision.
///
/// The prefix is the first [`HEARTBEAT_PREFIX_LEN`] bytes of the body
/// ([`Capabilities::decode_prefix`]).
///
/// [`ASSIGN_FLAG_BLE`]: super::ASSIGN_FLAG_BLE
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

    /// Read the prefix of any heartbeat body, whatever build sent it.
    ///
    /// Accepts a body at least [`HEARTBEAT_PREFIX_LEN`] bytes long, at any total length. The
    /// rest is a minor version's business and is not read. A host finds a node a minor version
    /// behind through it, so it can say so and update it.
    ///
    /// # Errors
    /// [`DecodeError::TooShort`] for a body shorter than the prefix.
    pub fn decode_prefix(body: &[u8]) -> Result<Self, DecodeError> {
        let Some(&[major, minor, patch, board, flags]) = body.first_chunk() else {
            return Err(DecodeError::TooShort { need: HEARTBEAT_PREFIX_LEN, got: body.len() });
        };
        Ok(Self::from_prefix([major, minor, patch, board, flags]))
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

/// Node → P4: a node's liveness, and who it is.
///
/// Sent every [`IDLE_BEAT_MS`] while unassigned and every [`ASSIGNED_BEAT_MS`] while assigned.
/// Never resent: a newer one replaces one the P4 has not clocked.
///
/// Twenty-three bytes: the [`Capabilities`] prefix, the node's MAC, then the counters and
/// readings below in order.
///
/// [`IDLE_BEAT_MS`]: crate::node::IDLE_BEAT_MS
/// [`ASSIGNED_BEAT_MS`]: crate::node::ASSIGNED_BEAT_MS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    /// What that node is.
    pub capabilities: Capabilities,
    /// The node's station MAC: its identity to the host, wherever it is plugged in.
    pub mac: Mac,
    /// Counts up from node boot: once per sweep, Bluetooth scan or idle beat. A value
    /// below the last one means the node restarted and has forgotten its assignment.
    pub counter: u32,
    /// The assignment epoch this node holds, or 0 for none (unassigned since boot). The
    /// host never puts 0 on the wire, so 0 is unambiguous.
    pub epoch: u8,
    /// Access points a full pending buffer turned away since boot, each once per dwell
    /// ([`crate::pending::Refused`]). Wraps. Most are reported on a later dwell, so this
    /// measures the buffer against the neighborhood rather than counting losses. The
    /// host reads it as a difference between heartbeats.
    pub wifi_refused: u16,
    /// Advertisers a full pending buffer turned away since boot, each once per scan.
    /// Wraps, and is read like `wifi_refused`. 0 on a node that has never held the
    /// Bluetooth scan.
    pub ble_refused: u16,
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

const P: usize = HEARTBEAT_PREFIX_LEN;
const M: usize = HEARTBEAT_IDENTITY_LEN;

impl Heartbeat {
    /// Decode a body.
    ///
    /// # Errors
    /// [`DecodeError::BadLength`] for any length but [`HEARTBEAT_LEN`].
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        fixed(body, HEARTBEAT_LEN)?;
        let (capabilities, mac) = Self::identity(body)?;
        let le = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
        Ok(Self {
            capabilities,
            mac,
            counter: le_u32(body, M),
            epoch: body[M + 4],
            wifi_refused: le(M + 5),
            ble_refused: le(M + 7),
            reset_cause: ResetCause::from_u8(body[M + 9]),
            vdd: Vdd(body[M + 10]),
            die_temp: DieTemp(body[M + 11]),
        })
    }

    /// Read the prefix and the MAC of any heartbeat body at least [`HEARTBEAT_IDENTITY_LEN`]
    /// long, so a node on another layout is still named by its address.
    ///
    /// # Errors
    /// [`DecodeError::TooShort`] for a shorter body.
    pub fn identity(body: &[u8]) -> Result<(Capabilities, Mac), DecodeError> {
        let Some(head) = body.first_chunk::<M>() else {
            return Err(DecodeError::TooShort { need: M, got: body.len() });
        };
        let capabilities = Capabilities::from_prefix([head[0], head[1], head[2], head[3], head[4]]);
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&head[P..M]);
        Ok((capabilities, mac))
    }

    /// Encode to the [`HEARTBEAT_LEN`] bytes a heartbeat's body is.
    #[must_use]
    pub fn encode(&self) -> [u8; HEARTBEAT_LEN] {
        let mut out = [0u8; HEARTBEAT_LEN];
        out[..P].copy_from_slice(&self.capabilities.to_prefix());
        out[P..M].copy_from_slice(&self.mac);
        out[M..M + 4].copy_from_slice(&self.counter.to_le_bytes());
        out[M + 4] = self.epoch;
        out[M + 5..M + 7].copy_from_slice(&self.wifi_refused.to_le_bytes());
        out[M + 7..M + 9].copy_from_slice(&self.ble_refused.to_le_bytes());
        out[M + 9] = self.reset_cause.as_u8();
        out[M + 10] = self.vdd.0;
        out[M + 11] = self.die_temp.0;
        out
    }
}

const _: () = assert!(M + 12 == HEARTBEAT_LEN);
