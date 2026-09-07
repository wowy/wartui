//! What the engine hands to the store.
//!
//! Plain owned data with no database types in it, so the engine stays free of
//! `rusqlite` and can be tested without one. The store's job is to turn these
//! into rows; deciding what is worth recording is the engine's.

use wartui_proto::air::RecordKind;
use wartui_proto::link::Mac;
use wartui_proto::plan::ChannelSet;

use crate::position::Fix;

/// A node was heard from, which is enough to keep its row current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSeen {
    /// The node's full six-byte MAC.
    ///
    /// Divergence 1: the vendor core keys nodes on a two-byte suffix
    /// (`src/WiFiOps.cpp:410-419`), which collides across a large enough fleet
    /// and silently merges two nodes' data.
    pub mac: Mac,
    /// Unix milliseconds when this host first saw the node in this session.
    pub first_seen_ms: i64,
    /// Unix milliseconds of the most recent frame of any kind.
    pub last_seen_ms: i64,
}

/// A node completed a sweep and announced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// Which node.
    pub node_mac: Mac,
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// The node's own counter, monotonic from its boot.
    pub counter: u32,
    /// How strongly the bridge heard it.
    pub link_rssi: Option<i8>,
}

/// One network or advertiser a node reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// Which node reported it.
    pub node_mac: Mac,
    /// Unix milliseconds of receipt by the bridge.
    pub rx_at_ms: i64,
    /// How strongly the bridge heard the reporting node — not the observed
    /// network. Kept because a node reporting from the edge of the mesh is
    /// worth being able to spot in the data afterwards.
    pub link_rssi: Option<i8>,
    /// The observed BSSID, six raw bytes.
    pub bssid: [u8; 6],
    /// Raw SSID bytes. Not a `String`: an SSID is whatever the access point
    /// beaconed, and forcing it through UTF-8 here would lose the original.
    pub ssid: Vec<u8>,
    /// The `AuthMode` token exactly as the node wrote it.
    pub security: String,
    /// Channel number, or 0 for BLE.
    pub channel: u16,
    /// Signal strength as the node measured it.
    pub rssi: i16,
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
    /// Where the host believed it was when this arrived.
    pub fix: Fix,
    /// The line as it came off the air.
    ///
    /// About eighty bytes a row, and the reason a parser fix can be applied to
    /// history rather than only to what arrives afterwards. The wire format is
    /// undocumented and read out of someone else's C++; assuming this decoder
    /// is right forever would be optimistic.
    pub raw_text: Vec<u8>,
}

/// A frame exactly as it arrived, kept only when `--record-raw` is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// Transmitter.
    pub src: Mac,
    /// Destination, usually broadcast.
    pub dst: Mac,
    /// Link signal strength.
    pub rssi: Option<i8>,
    /// Channel the bridge was parked on.
    pub channel: Option<u8>,
    /// The undecoded payload.
    pub bytes: Vec<u8>,
}

/// Which bridge this capture came through.
///
/// Separate from [`crate::store::SessionInfo`] because a session is opened
/// before any bridge has announced itself: the store cannot wait for one, and a
/// capture that never finds a dongle still deserves a session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSeen {
    /// The bridge's own MAC, which nodes see as the core's address.
    pub mac: Mac,
    /// Which chip it is.
    pub chip: String,
    /// Its firmware version.
    pub fw_version: String,
}

/// What became of one `MSG_ADMIN` this host put on the air.
///
/// The vendor core has no equivalent: it clears its dirty flag from the
/// `esp_now_send` return value and keeps no record of whether anything
/// arrived. Every attempt gets a row here, successful or not, which is what
/// turns "the fleet keeps drifting off its channels" into a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOutcome {
    /// The node's radio acknowledged the frame at the MAC layer.
    Acked,
    /// It went out and nothing came back. Usually BLE coexistence on the node:
    /// NimBLE and Wi-Fi share the one 2.4 GHz antenna, and the admin window is
    /// precisely when the node would otherwise be idle.
    Unacked,
    /// The bridge could not transmit it at all.
    Refused,
    /// No answer came back from the bridge inside the timeout, so we do not
    /// know. Distinct from `Unacked`, where the radio did report.
    Silent,
}

impl AdminOutcome {
    /// The token stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acked => "acked",
            Self::Unacked => "unacked",
            Self::Refused => "refused",
            Self::Silent => "silent",
        }
    }
}

/// One assignment this host transmitted, and what came of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignmentSent {
    /// Which node it was addressed to.
    pub node_mac: Mac,
    /// The persisted monotonic counter this assignment was allocated from.
    /// Divergence 4: the vendor core keeps this in RAM and resets it at boot.
    pub counter: u64,
    /// The byte that actually went on the wire, `wire_version(counter)`.
    pub wire_version: u8,
    /// The node's slot in the fleet-wide stagger order.
    pub node_index: u8,
    /// Fleet size as of the assignment, not as of the send. Divergence 7: the
    /// vendor core reads it live (`src/WiFiOps.cpp:651`), so a node joining
    /// between planning and sending gets a count that disagrees with the
    /// partition its share came from.
    pub node_count: u8,
    /// Which `SCAN_CHANNELS` indices the node was told to dwell on.
    pub channels: ChannelSet,
    /// Whether it was also told to scan Bluetooth.
    pub ble: bool,
    /// Unix milliseconds the frame was handed to the link.
    pub created_at_ms: i64,
    /// Unix milliseconds the outcome arrived, if one did.
    pub delivered_at_ms: Option<i64>,
    /// What the radio said.
    pub outcome: AdminOutcome,
    /// Bridge-measured microseconds from the heartbeat that opened the node's
    /// admin window to the transmit callback. The whole reason the bridge
    /// stamps both ends itself: the host's own scheduling noise never enters
    /// the number.
    pub latency_us: Option<u32>,
}

/// Anything the engine wants written down.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// Record which bridge this session is running through.
    Bridge(BridgeSeen),
    /// Insert or refresh a node row.
    Node(NodeSeen),
    /// Append a heartbeat.
    Heartbeat(Heartbeat),
    /// Append an observation.
    Observation(Observation),
    /// Append an undecoded frame.
    Raw(RawFrame),
    /// Append an assignment this host transmitted.
    Assignment(AssignmentSent),
}
