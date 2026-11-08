use super::DecodeError;
use super::msg::fixed;
use crate::plan::{CHANNEL_SET_BYTES, ChannelSet};

/// Length of an [`Assign`] body.
pub const ASSIGN_LEN: usize = 2 + CHANNEL_SET_BYTES;

/// Length of a [`Clear`] body: nothing.
pub const CLEAR_LEN: usize = 0;

/// [`Assign::flags`] bit 0: scan Bluetooth.
pub const ASSIGN_FLAG_BLE: u8 = 1 << 0;

/// P4 → node: a channel and Bluetooth assignment. One of the two messages that change what a
/// node scans; [`Clear`] is the other.
///
/// Eight bytes: [`epoch`](Self::epoch), [`flags`](Self::flags) and six bytes of
/// [`ChannelSet`].
///
/// The channels are a mask, not a pair of bounds. A run cannot describe a restricted
/// pool: the US pool is 2.4 GHz 1-11 and 5 GHz 36-165 with a gap between. A mask says
/// it in one message, and lets the planner deal round-robin so every node carries some of
/// both bands.
///
/// An assignment counts as delivered only on the bus ack of the transfer that carried it, and
/// as adopted only once a heartbeat reports its epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assign {
    /// Epoch counter. A node adopts the assignment only when this *differs* from the
    /// one it holds. Never 0 on the wire.
    pub epoch: u8,
    /// Per-node switches. [`ASSIGN_FLAG_BLE`] is the only one defined.
    ///
    /// Carried whole rather than unpacked into `bool`s, so an unknown bit survives a
    /// decode and re-encode. [`Security::Unknown`] exists for the same reason.
    ///
    /// [`Security::Unknown`]: super::Security::Unknown
    pub flags: u8,
    /// Which [`SCAN_CHANNELS`](crate::plan::SCAN_CHANNELS) indices to dwell on.
    pub channels: ChannelSet,
}

impl Assign {
    /// Whether this assignment asks the node to scan Bluetooth.
    #[must_use]
    pub const fn scan_ble(&self) -> bool {
        self.flags & ASSIGN_FLAG_BLE != 0
    }

    /// The flags byte for a node that should or should not scan Bluetooth.
    #[must_use]
    pub const fn flags_for(scan_ble: bool) -> u8 {
        if scan_ble { ASSIGN_FLAG_BLE } else { 0 }
    }

    /// Decode a body.
    ///
    /// # Errors
    /// [`DecodeError::BadLength`] for any length but [`ASSIGN_LEN`].
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        fixed(body, ASSIGN_LEN)?;
        let mut channels = [0u8; CHANNEL_SET_BYTES];
        channels.copy_from_slice(&body[2..ASSIGN_LEN]);
        Ok(Self { epoch: body[0], flags: body[1], channels: ChannelSet::from_bytes(channels) })
    }

    /// Encode to the [`ASSIGN_LEN`] bytes of its body.
    #[must_use]
    pub fn encode(&self) -> [u8; ASSIGN_LEN] {
        let mut out = [0u8; ASSIGN_LEN];
        out[0] = self.epoch;
        out[1] = self.flags;
        out[2..].copy_from_slice(&self.channels.to_bytes());
        out
    }
}

/// P4 → node: forget every address it has reported.
///
/// An empty body; the type is the whole instruction. It carries no epoch because it is
/// idempotent. Clearing an empty ring again costs one extra round of sightings, so there is
/// no state for the two ends to disagree about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clear;

impl Clear {
    /// Decode a body.
    ///
    /// # Errors
    /// [`DecodeError::BadLength`] for a body that is not empty.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        fixed(body, CLEAR_LEN)?;
        Ok(Self)
    }

    /// The empty body.
    #[must_use]
    pub const fn encode(&self) -> [u8; CLEAR_LEN] {
        []
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
