use super::{DecodeError, MsgType, OFF_BODY, header, write_header};
use crate::plan::{CHANNEL_SET_BYTES, ChannelSet};

/// Length of [`AdminMsg`] on the wire.
pub const ADMIN_MSG_LEN: usize = OFF_BODY + 2 + CHANNEL_SET_BYTES + 1;

/// Length of [`ClearMsg`] on the wire: the header, and nothing else.
pub const CLEAR_MSG_LEN: usize = OFF_BODY;

/// [`AdminMsg::flags`] bit 0: scan Bluetooth as well as Wi-Fi.
pub const ADMIN_FLAG_BLE: u8 = 1 << 0;

/// wartui's channel assignment — one of the two frames a node acts on, the
/// other being [`ClearMsg`].
///
/// Fifteen bytes: the header, then [`epoch`](Self::epoch), [`flags`](Self::flags),
/// six bytes of [`ChannelSet`] and transmit power.
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
    ///
    /// [`WIRE_VERSION`]: super::WIRE_VERSION
    pub epoch: u8,
    /// Per-node switches. [`ADMIN_FLAG_BLE`] is the only one defined.
    ///
    /// Carried whole rather than unpacked into `bool`s so an unknown bit set by
    /// a newer host survives a decode and re-encode instead of being quietly
    /// dropped — the same reason [`Security::Unknown`] exists.
    ///
    /// [`Security::Unknown`]: super::Security::Unknown
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
        channels.copy_from_slice(&buf[OFF_BODY + 2..OFF_BODY + 2 + CHANNEL_SET_BYTES]);
        Ok(Self {
            epoch: buf[OFF_BODY],
            flags: buf[OFF_BODY + 1],
            channels: ChannelSet::from_bytes(channels),
            tx_power: i8::from_le_bytes([buf[OFF_BODY + 2 + CHANNEL_SET_BYTES]]),
        })
    }

    /// Encode to the [`ADMIN_MSG_LEN`] bytes a wartui node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; ADMIN_MSG_LEN] {
        let mut out = [0u8; ADMIN_MSG_LEN];
        write_header(&mut out, MsgType::Admin);
        out[OFF_BODY] = self.epoch;
        out[OFF_BODY + 1] = self.flags;
        out[OFF_BODY + 2..OFF_BODY + 2 + CHANNEL_SET_BYTES]
            .copy_from_slice(&self.channels.to_bytes());
        out[OFF_BODY + 2 + CHANNEL_SET_BYTES] = self.tx_power.to_le_bytes()[0];
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
