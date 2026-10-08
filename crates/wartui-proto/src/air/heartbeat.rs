use core::fmt;

use super::{DecodeError, MsgType, OFF_BODY, header, write_header};

/// Length of [`HeartbeatMsg`] on the wire.
pub const HEARTBEAT_MSG_LEN: usize = OFF_BODY + 17;

/// [`Capabilities::flags`] bit 0: this radio reaches 5 GHz.
pub const CAP_FLAG_5G: u8 = 1 << 0;

/// What a node says it is, in every heartbeat it sends.
///
/// A version, and the one capability that varies by board.
///
/// `five_ghz` is a refusal, not a request. A node without it is never dealt a 5 GHz
/// index, because a share it cannot tune is a share nobody scans. Bluetooth is not a
/// capability: every node can scan it, and [`ADMIN_FLAG_BLE`] is the operator's
/// decision.
///
/// [`ADMIN_FLAG_BLE`]: super::ADMIN_FLAG_BLE
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The node protocol's major version. Distinct from [`WIRE_VERSION`], which covers
    /// the frame around it.
    ///
    /// [`WIRE_VERSION`]: super::WIRE_VERSION
    pub major: u8,
    /// The node protocol's minor version.
    pub minor: u8,
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
    pub const fn here(five_ghz: bool) -> Self {
        Self { major: CAPABILITY_MAJOR, minor: CAPABILITY_MINOR, five_ghz }
    }

    /// The feature bits as the wire carries them.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        if self.five_ghz { CAP_FLAG_5G } else { 0 }
    }

    /// Rebuild from the three bytes a heartbeat carries.
    ///
    /// Unknown flag bits are ignored, not refused. A node must not stop being a node
    /// because it sets a bit this build does not know.
    #[must_use]
    pub const fn from_parts(major: u8, minor: u8, flags: u8) -> Self {
        Self { major, minor, five_ghz: flags & CAP_FLAG_5G != 0 }
    }
}

/// The form the fleet table and the store column show: `wartui/1.0;5g`. Only the three
/// bytes travel; this is for reading.
impl fmt::Display for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wartui/{}.{}", self.major, self.minor)?;
        if self.five_ghz {
            f.write_str(";5g")?;
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
        let le = |at: usize| u16::from_le_bytes([buf[at], buf[at + 1]]);
        Ok(Self {
            counter,
            epoch: buf[OFF_BODY + 4],
            capabilities: Capabilities::from_parts(
                buf[OFF_BODY + 5],
                buf[OFF_BODY + 6],
                buf[OFF_BODY + 7],
            ),
            wifi_refused: le(OFF_BODY + 8),
            ble_refused: le(OFF_BODY + 10),
            beat: le(OFF_BODY + 12),
            unsent: buf[OFF_BODY + 14],
            dwell: buf[OFF_BODY + 15],
            prev_dwell: buf[OFF_BODY + 16],
        })
    }

    /// Encode to the twenty-three bytes a heartbeat is.
    #[must_use]
    pub fn encode(&self) -> [u8; HEARTBEAT_MSG_LEN] {
        let mut out = [0u8; HEARTBEAT_MSG_LEN];
        write_header(&mut out, MsgType::Heartbeat);
        out[OFF_BODY..OFF_BODY + 4].copy_from_slice(&self.counter.to_le_bytes());
        out[OFF_BODY + 4] = self.epoch;
        out[OFF_BODY + 5] = self.capabilities.major;
        out[OFF_BODY + 6] = self.capabilities.minor;
        out[OFF_BODY + 7] = self.capabilities.flags();
        out[OFF_BODY + 8..OFF_BODY + 10].copy_from_slice(&self.wifi_refused.to_le_bytes());
        out[OFF_BODY + 10..OFF_BODY + 12].copy_from_slice(&self.ble_refused.to_le_bytes());
        out[OFF_BODY + 12..OFF_BODY + 14].copy_from_slice(&self.beat.to_le_bytes());
        out[OFF_BODY + 14] = self.unsent;
        out[OFF_BODY + 15] = self.dwell;
        out[OFF_BODY + 16] = self.prev_dwell;
        out
    }
}
