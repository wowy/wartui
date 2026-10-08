use super::{DecodeError, MsgType, OFFSET_BODY, header, write_header};
use crate::plan::{CHANNEL_SET_BYTES, ChannelSet};

/// Length of [`AdminMsg`] on the wire.
pub const ADMIN_MSG_LEN: usize = OFFSET_BODY + 2 + CHANNEL_SET_BYTES + 1;

/// Length of [`ClearMsg`] on the wire: the header, and nothing else.
pub const CLEAR_MSG_LEN: usize = OFFSET_BODY;

/// [`AdminMsg::flags`] bit 0: scan Bluetooth as well as Wi-Fi.
pub const ADMIN_FLAG_BLE: u8 = 1 << 0;

/// Host → node: a channel and Bluetooth assignment. One of the two frames a node acts
/// on; [`ClearMsg`] is the other.
///
/// Fifteen bytes: the header, then [`epoch`](Self::epoch), [`flags`](Self::flags), six
/// bytes of [`ChannelSet`] and transmit power.
///
/// The channels are a mask, not a pair of bounds. A run cannot describe a restricted
/// pool: the US pool is 2.4 GHz 1-11 and 5 GHz 36-165 with a gap between. A mask says
/// it in one frame, and lets the planner deal round-robin so every node carries some of
/// both bands.
///
/// Delivery follows measured behavior (`docs/phase-0-findings.md`). The radio retries
/// an unacknowledged unicast 31 times, all inside the one window that failed. An 802.11
/// acknowledgment comes from the receiver's MAC hardware, so a missing one means the
/// node's radio was not on the channel, for instance because Bluetooth held the shared
/// antenna. Hence three rules:
///
/// - An assignment counts as sent only on the transmit callback's ack.
/// - A failed one is retried on the node's next heartbeat, not by trusting the radio's
///   own retries.
/// - A node that stays unacknowledged is reported as likely Bluetooth coexistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminMsg {
    /// Epoch counter. A node adopts the assignment only when this *differs* from the
    /// one it holds. Never 0 on the wire.
    ///
    /// Distinct from the header's [`WIRE_VERSION`], which is the shape of the frame
    /// rather than the generation of the plan inside it.
    ///
    /// [`WIRE_VERSION`]: super::WIRE_VERSION
    pub epoch: u8,
    /// Per-node switches. [`ADMIN_FLAG_BLE`] is the only one defined.
    ///
    /// Carried whole rather than unpacked into `bool`s, so an unknown bit survives a
    /// decode and re-encode. [`Security::Unknown`] exists for the same reason.
    ///
    /// [`Security::Unknown`]: super::Security::Unknown
    pub flags: u8,
    /// Which [`SCAN_CHANNELS`](crate::plan::SCAN_CHANNELS) indices to dwell on.
    pub channels: ChannelSet,
    /// Wi-Fi transmit power in ESP-IDF quarter-dBm units.
    ///
    /// Applied when the assignment is adopted, with the rest of the node's work.
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
        channels.copy_from_slice(&buf[OFFSET_BODY + 2..OFFSET_BODY + 2 + CHANNEL_SET_BYTES]);
        Ok(Self {
            epoch: buf[OFFSET_BODY],
            flags: buf[OFFSET_BODY + 1],
            channels: ChannelSet::from_bytes(channels),
            tx_power: i8::from_le_bytes([buf[OFFSET_BODY + 2 + CHANNEL_SET_BYTES]]),
        })
    }

    /// Encode to the [`ADMIN_MSG_LEN`] bytes a wartui node expects.
    #[must_use]
    pub fn encode(&self) -> [u8; ADMIN_MSG_LEN] {
        let mut out = [0u8; ADMIN_MSG_LEN];
        write_header(&mut out, MsgType::Admin);
        out[OFFSET_BODY] = self.epoch;
        out[OFFSET_BODY + 1] = self.flags;
        out[OFFSET_BODY + 2..OFFSET_BODY + 2 + CHANNEL_SET_BYTES]
            .copy_from_slice(&self.channels.to_bytes());
        out[OFFSET_BODY + 2 + CHANNEL_SET_BYTES] = self.tx_power.to_le_bytes()[0];
        out
    }
}

/// Host → node: forget every address it has reported.
///
/// Header only; the type byte is the whole instruction. It carries no epoch because it
/// is idempotent. Clearing an empty ring again costs one extra round of sightings, so
/// there is no state for the two ends to disagree about.
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
/// A node adopts an assignment only when this byte *differs* from the one it holds, so
/// an epoch reused after a host restart is silently discarded. The host therefore
/// persists a `u64` counter and narrows it here. Zero is skipped, because a node's 0
/// means it holds no assignment.
#[must_use]
pub const fn wire_epoch(counter: u64) -> u8 {
    // `% 255` lands in 0..=254; the offset moves that to 1..=255.
    ((counter.wrapping_sub(1) % 255) as u8) + 1
}
