//! What the engine hands to the store: plain owned data with no database types, so the engine stays
//! free of `rusqlite` and testable without it. The store turns these into rows. The engine decides
//! what is worth recording.
//!
//! [`ssid_text`] lives here, not beside either caller, because both read this module's bytes and
//! two copies drifted apart.

use std::time::Duration;

use wartui_proto::air::RecordKind;
use wartui_proto::beacon::visible_ssid;
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, ChannelSet};

use crate::position::Fix;

/// Raw SSID bytes as text, for showing to a person or writing into a column parsed as text.
///
/// One rule for the view and the export. [`visible_ssid`] strips a cloaked access point's padding
/// here as well as in the parser, so every path from bytes to text gets it. Any NUL left is
/// interior, and gets what `from_utf8_lossy` gives any byte it cannot represent: a raw NUL is no
/// more use to a terminal than to WiGLE's parser. The store keeps whatever arrived.
/// is no more use to a terminal than to WiGLE's parser. The store keeps whatever arrived.
#[must_use]
pub fn ssid_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(visible_ssid(bytes));
    if text.contains('\0') { text.replace('\0', "\u{FFFD}") } else { text.into_owned() }
}

/// A node was heard from, which is enough to keep its row current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSeen {
    /// The node's full six-byte MAC. A shorter suffix collides in a large enough fleet and merges
    /// two nodes' data.
    pub mac: Mac,
    /// Unix milliseconds when this host first saw the node in this capture.
    pub first_seen_ms: i64,
    /// Unix milliseconds of the most recent frame of any kind.
    pub last_seen_ms: i64,
    /// The capability token from this node's most recent heartbeat, verbatim, or `None` for a frame
    /// without one.
    ///
    /// Kept as wire text, so a capture says what a node claimed to be even in a version this build
    /// cannot parse. It is also the only record of why a node was never assigned: heartbeats, no
    /// token and no assignments is the diagnosis. The store coalesces `None` rather than
    /// overwriting, so the column holds the last token ever seen (the `node` upsert in
    /// `write_batch` says why).
    pub capabilities: Option<String>,
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
    /// The assignment epoch the node says it holds, 0 for none, as the frame carried it, replay
    /// included.
    pub epoch: u8,
    /// How strongly the bridge heard it.
    pub link_rssi: Option<i8>,
    /// Access points the node's full pending buffer turned away since boot, once per dwell each.
    pub wifi_refused: u16,
    /// Advertisers the node's full pending buffer turned away since boot, once per scan each.
    pub ble_refused: u16,
    /// Heartbeats the node has sent since its boot, this one included. Wraps.
    pub beat: u16,
    /// Heartbeats the node's radio refused to send since its boot, this one excluded. Wraps.
    pub unsent: u8,
    /// Whether it arrived live rather than replayed from the bridge's backlog.
    pub live: bool,
}

/// One status reply from the bridge, as sent. Counters are the bridge's since boot, so a reboot
/// shows as values falling. The engine's since-attach figure is not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStatusSeen {
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// How many peers its ESP-NOW table holds.
    pub peer_count: u8,
    /// Frames it has received since its boot.
    pub rx_count: u32,
    /// Frames its outbound ring dropped on the way to the host since its boot.
    pub dropped_tx: u32,
    /// Milliseconds since its boot.
    pub uptime_ms: u32,
    /// Frames this host had read off the link when the reply arrived (`Counters::frames`). Between
    /// two rows, the `rx_count` difference less this one's is USB loss plus `dropped_tx`, give or
    /// take frames still queued in the bridge, which the reply overtakes.
    pub host_frames: u64,
}

/// The host's own state, sampled every few seconds and at shutdown.
///
/// Counts run from the engine's start, so two rows are read as a difference. Peaks are the largest
/// since the previous row, so rows show *when* the host struggled. A row lost to a full queue costs
/// only that window's peaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostStatus {
    /// Unix milliseconds of the sample.
    pub at_ms: i64,
    /// `Counters::frames`: ESP-NOW frames read off the link.
    pub frames: u64,
    /// `Counters::duplicate_batches`: radio retransmits dropped rather than stored.
    pub duplicate_batches: u64,
    /// `Counters::garbled`: USB frames that failed their checksum.
    pub garbled: u64,
    /// `Counters::undecodable`.
    pub undecodable: u64,
    /// `Counters::incompatible`.
    pub incompatible: u64,
    /// `Counters::foreign_fleet`.
    pub foreign_fleet: u64,
    /// `Counters::foreign_admin`.
    pub foreign_admin: u64,
    /// `Counters::admin_windows_missed`.
    pub admin_windows_missed: u64,
    /// The furthest behind the air this host fell since the previous row, in microseconds. Only
    /// lags measured from arriving frames count, not the assumption made on connecting.
    pub lag_peak_us: u64,
    /// Rows the store has written.
    pub store_written: u64,
    /// Rows the store has dropped.
    pub store_dropped: u64,
    /// The deepest the store's queue got since the previous row.
    pub store_queue_peak: u64,
    /// The slowest batch since the previous row, statements and commit together, in microseconds.
    pub store_commit_peak_us: u64,
    /// The Raspberry Pi firmware's throttle word, `None` off a Pi ([`crate::health`] has its bits).
    pub throttled: Option<u32>,
    /// SoC temperature in thousandths of a degree Celsius, or `None` when unreadable.
    pub soc_temp_mc: Option<i32>,
    /// Battery voltage in millivolts, or `None` when the host has no `battery` hwmon.
    pub battery_mv: Option<i32>,
    /// Battery current in milliamps, signed as the driver reports it, `None` as for `battery_mv`.
    pub battery_ma: Option<i32>,
}

/// A run of batches missing from one node's sequence, found when the next batch arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchGap {
    /// Which node.
    pub node_mac: Mac,
    /// Unix milliseconds the batch after the gap arrived, not when the lost ones were sent.
    pub rx_at_ms: i64,
    /// The `seq` of the last batch before the gap.
    pub after_seq: u16,
    /// The `seq` of the batch that revealed it.
    pub seq: u16,
    /// Batches missing between the two: `seq - after_seq - 1` modulo 2^16, from 1 to 1023.
    pub lost: u16,
}

/// One network or advertiser a node reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// Which node reported it.
    pub node_mac: Mac,
    /// Unix milliseconds of receipt by the bridge.
    pub rx_at_ms: i64,
    /// How strongly the bridge heard the reporting node, not the network, to spot nodes at the
    /// mesh's edge.
    pub link_rssi: Option<i8>,
    /// The observed BSSID, six raw bytes.
    pub bssid: [u8; 6],
    /// Raw SSID bytes. An SSID is whatever the access point beaconed, so UTF-8 here would lose the
    /// original. [`ssid_text`] turns it into text.
    pub ssid: Vec<u8>,
    /// The `AuthMode` token exactly as the node wrote it.
    pub security: String,
    /// Channel number, or 0 for BLE.
    pub channel: u16,
    /// Signal strength as the node measured it.
    pub rssi: i16,
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
    /// The roaming consortium element's body, verbatim. `None` for BLE, and for the Wi-Fi without
    /// one, which is most.
    pub rcoi: Option<Vec<u8>>,
    /// The Bluetooth SIG company identifier from a BLE advertiser's manufacturer data. `None` for
    /// Wi-Fi, and for the many advertisers that send none.
    pub mfgr_id: Option<u16>,
    /// Where the host believed it was when this arrived.
    pub fix: Fix,
    /// This record's own bytes, without the batch header, which a re-parse does not need. A couple
    /// of dozen bytes a row, and the reason a decoder fix can reach history, not only what arrives
    /// later. `--record-raw` keeps whole frames as [`RawFrame`]s.
    pub raw_body: Vec<u8>,
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
    /// The undecoded payload.
    pub bytes: Vec<u8>,
}

/// Which bridge this capture came through. Separate from [`crate::store::CaptureInfo`] because a
/// capture is created before any bridge announces itself, and one that never finds a dongle still
/// gets a capture row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSeen {
    /// The bridge's own MAC, which nodes see as the core's address.
    pub mac: Mac,
    /// Which chip it is.
    pub chip: String,
    /// Its firmware version.
    pub fw_version: String,
}

/// What became of one assignment this host put on the air. Every attempt gets a row, so "the fleet
/// keeps drifting off its channels" becomes a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOutcome {
    /// The node's radio acknowledged the frame at the MAC layer.
    Acked,
    /// It went out and nothing came back. Usually BLE coexistence on the node.
    Unacked,
    /// The bridge could not transmit it at all.
    Refused,
    /// No answer from the bridge inside the timeout, so unknown. Unlike `Unacked`, where the radio
    /// did report.
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
    /// The engine's epoch counter it was allocated from: monotonic within a capture, not persisted.
    pub counter: u64,
    /// The byte that went on the wire, `air::wire_epoch(counter)`.
    pub wire_version: u8,
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
    /// Bridge-measured microseconds from the heartbeat that opened the node's window to the
    /// transmit callback, both ends stamped by the bridge. `None` with no heartbeat stamp, or a
    /// difference longer than the window, meaning that heartbeat opened none. The outcome is
    /// recorded either way.
    pub latency_us: Option<u32>,
}

/// Host settings that affect capture interpretation, not proof that a radio adopted them.
///
/// Kept separately from assignments: power and pool can change before any node is assignable,
/// and the bridge's configured power never rides a node assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSettings {
    /// Pool the planner partitions.
    pub pool: ChannelPool,
    /// Normalized desired node power, in quarter-dBm.
    pub tx_power: i8,
    /// Normalized desired bridge power, in quarter-dBm.
    pub bridge_tx_power: i8,
    /// Whether undecoded frames are retained.
    pub record_raw: bool,
    /// Whether the position chain has a GPS receiver.
    pub gps: bool,
    /// Fix arrival-age limit, independent of receiver timestamps.
    pub gps_max_age: Duration,
    /// Whether Bluetooth choices are remembered.
    pub remember_ble: bool,
    /// Remembered Bluetooth node, if enabled.
    pub preferred_ble: Option<Mac>,
    /// Node the host currently gives the Bluetooth job, if any.
    pub ble_node: Option<Mac>,
    /// Heartbeat age that removes a node from the topology.
    pub topology_timeout: Duration,
    /// Bridge counter polling cadence.
    pub status_interval: Duration,
    /// Assignment outcome timeout.
    pub admin_timeout: Duration,
}

/// Anything the engine or runtime wants written down.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// Append applied host settings. Sequence is arrival order even if the wall clock steps back.
    CaptureSettings { sequence: u64, at_ms: i64, settings: CaptureSettings },
    /// Record which bridge this capture is running through.
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
    /// Append a bridge status reply.
    BridgeStatus(BridgeStatusSeen),
    /// Append a gap in a node's batch sequence.
    BatchGap(BatchGap),
    /// Append a sample of the host's own state.
    HostStatus(HostStatus),
}

#[cfg(test)]
mod tests {
    use super::ssid_text;

    #[test]
    fn ssid_text_strips_padding_when_ssid_cloaked() {
        assert_eq!(ssid_text(&[0u8; 8]), "");
        assert_eq!(ssid_text(b"Home\0\0\0"), "Home");
        assert_eq!(ssid_text(b""), "");
        assert_eq!(ssid_text(b"plain"), "plain");
    }

    #[test]
    fn ssid_text_replaces_nul_when_interior() {
        // The trim leaves an interior NUL, so this is the last guard before a column WiGLE parses,
        // or a terminal that would swallow it and show a shorter name.
        assert_eq!(ssid_text(b"a\0b"), "a\u{FFFD}b");
    }
}
