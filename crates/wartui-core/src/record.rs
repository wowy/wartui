//! What the engine hands to the store: plain owned data with no database types, so the engine stays
//! free of `rusqlite` and testable without it. The store turns these into rows. The engine decides
//! what is worth recording.
//!
//! [`ssid_text`] lives here, not beside either caller, because both read this module's bytes and
//! two copies drifted apart.

use std::time::Duration;

use wartui_proto::beacon::visible_ssid;
use wartui_proto::bus::{NodeBoard, RecordKind, Slot};
use wartui_proto::fan::FanStats;
use wartui_proto::link::{QueueStats, RadioLinkStats};
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, ChannelSet};
use wartui_proto::reset::ResetCause;

use crate::position::Fix;

/// Raw SSID bytes as text, for showing to a person or writing into a column parsed as text.
///
/// One rule for the view and the export. [`visible_ssid`] strips a cloaked access point's padding
/// here as well as in the parser, so every path from bytes to text gets it. Each control character
/// left, NUL included, becomes U+FFFD, the character `from_utf8_lossy` gives invalid bytes. A
/// terminal and WiGLE's parser have no use for one, and an escape drives the terminal. The store
/// keeps whatever arrived.
#[must_use]
pub fn ssid_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(visible_ssid(bytes));
    if text.contains(char::is_control) {
        text.chars().map(|c| if c.is_control() { '\u{FFFD}' } else { c }).collect()
    } else {
        text.into_owned()
    }
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
    /// The assignment epoch the node says it holds, 0 for none, as the heartbeat carried it,
    /// replay included.
    pub epoch: u8,
    /// The node-bus slot it came from.
    pub slot: Slot,
    /// Access points the node's full pending buffer turned away since boot, once per dwell each.
    pub wifi_refused: u16,
    /// Advertisers the node's full pending buffer turned away since boot, once per scan each.
    pub ble_refused: u16,
    /// Why the node is running this boot; the same on every heartbeat of a boot.
    pub reset_cause: ResetCause,
    /// The node chip's 3.3 V rail in millivolts, `None` when the read failed.
    pub vdd_mv: Option<u16>,
    /// The node's die temperature in whole degrees Celsius, `None` when the read failed.
    pub die_temp_c: Option<i16>,
    /// Bytes free in the node's heap.
    pub heap_free: u32,
    /// Whether it arrived live rather than replayed from the bridge's backlog.
    pub live: bool,
}

/// One status reply from the bridge, as sent. Counters are the bridge's since boot, so a reboot
/// shows as values falling. The engine's since-attach figure is not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatusSeen {
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// Node records it has forwarded since its boot.
    pub rx_count: u32,
    /// Frames its outbound ring dropped on the way to the host since its boot.
    pub dropped_tx: u32,
    /// Milliseconds since its boot.
    pub uptime_ms: u32,
    /// Node records this host had read off the link when the reply arrived (`Counters::frames`).
    /// Between two rows, the `rx_count` difference less this one's is link loss plus `dropped_tx`,
    /// give or take records still queued in the bridge, which the reply overtakes.
    pub host_frames: u64,
    /// Node records too long to forward that it dropped since its boot. Not in `rx_count`.
    pub rx_oversize: u32,
    /// Its queues' capacity, high-water mark and drops since its boot.
    pub queues: QueueStats,
    /// Its radio link's counters since its boot.
    pub radio_link: RadioLinkStats,
    /// Line errors on the P4's host UART since its boot: bytes from this host lost or corrupted.
    pub host_uart_errors: u32,
    /// Its lid fan: the duty in force and the last tach reading. `None` with no carrier.
    pub fan: Option<FanStats>,
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
    /// `Counters::frames`: node records read off the link.
    pub frames: u64,
    /// `Counters::duplicate_batches`: copies of a batch dropped rather than stored.
    pub duplicate_batches: u64,
    /// `Counters::garbled`: USB frames that failed their checksum.
    pub garbled: u64,
    /// `Counters::undecodable`.
    pub undecodable: u64,
    /// `Counters::incompatible`.
    pub incompatible: u64,
    /// The furthest behind the bridge this host fell since the previous row, in microseconds. Only
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
    /// The node-bus slot the batch came from.
    pub slot: Slot,
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
    /// later. `--record-raw` keeps whole records as [`RawFrame`]s.
    pub raw_body: Vec<u8>,
}

/// A node record exactly as it arrived, kept only when `--record-raw` is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// The node-bus slot it came from.
    pub slot: Slot,
    /// Its record type, a `bus::UpType` byte.
    pub kind: u8,
    /// The undecoded body.
    pub bytes: Vec<u8>,
}

/// Which bridge this capture came through. Separate from [`crate::store::CaptureInfo`] because a
/// capture is created before any bridge announces itself, and one that never finds a dongle still
/// gets a capture row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSeen {
    /// The bridge's MAC: its C6's, which names the bridge.
    pub mac: Mac,
    /// Its firmware version.
    pub fw_version: String,
    /// What carries its nodes, which maps the stored bus slots to the silk names.
    pub node_board: NodeBoard,
}

/// What became of one assignment this host sent. Every attempt gets a row, so "the fleet keeps
/// drifting off its channels" becomes a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOutcome {
    /// The bridge delivered it: the node acknowledged the transfer that carried it.
    Acked,
    /// The bridge could not deliver it: no node in the slot, a full queue, or no ack after every
    /// try.
    Unacked,
    /// No answer from the bridge inside the timeout, so unknown. Unlike `Unacked`, where the
    /// bridge did report.
    Silent,
}

impl AdminOutcome {
    /// The token stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acked => "acked",
            Self::Unacked => "unacked",
            Self::Silent => "silent",
        }
    }
}

/// One assignment this host sent, and what came of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignmentSent {
    /// Which node it was addressed to.
    pub node_mac: Mac,
    /// The engine's epoch counter it was allocated from: monotonic within a capture, not persisted.
    pub counter: u64,
    /// The byte that went on the wire, `bus::wire_epoch(counter)`.
    pub wire_version: u8,
    /// Which `SCAN_CHANNELS` indices the node was told to dwell on.
    pub channels: ChannelSet,
    /// Whether it was also told to scan Bluetooth.
    pub ble: bool,
    /// Unix milliseconds the assignment was handed to the link.
    pub created_at_ms: i64,
    /// Unix milliseconds the outcome arrived, if one did.
    pub delivered_at_ms: Option<i64>,
    /// What the bridge said.
    pub outcome: AdminOutcome,
    /// Microseconds from sending it to the bridge's `Delivered`, on this host's clock. `None`
    /// when it was not delivered.
    pub latency_us: Option<u32>,
}

/// Host settings that affect capture interpretation, not proof that a radio adopted them.
///
/// Kept separately from assignments: the pool can change before any node is assignable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSettings {
    /// Pool the planner partitions, as the bridge reports it. `None` before its first `Status`.
    pub pool: Option<ChannelPool>,
    /// Whether undecoded node records are retained.
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
    /// Append an undecoded node record.
    Raw(RawFrame),
    /// Append an assignment this host sent.
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
    fn ssid_text_replaces_control_characters_when_interior() {
        // The trim leaves interior control characters, so this is the last guard before WiGLE's
        // parser and the terminal.
        assert_eq!(ssid_text(b"a\0b"), "a\u{FFFD}b");
        assert_eq!(ssid_text(&[0x04, 0x00, 0x01]), "\u{FFFD}\u{FFFD}\u{FFFD}");
        assert_eq!(ssid_text(b"a\x1b[2Jb"), "a\u{FFFD}[2Jb");
        assert_eq!(ssid_text(b"a\nb\x7f"), "a\u{FFFD}b\u{FFFD}");
        assert_eq!(ssid_text(&[b'a', 0xC2, 0x9B, b'b']), "a\u{FFFD}b");
    }

    #[test]
    fn ssid_text_keeps_text_when_printable() {
        // Non-control Unicode passes through unchanged.
        assert_eq!(ssid_text("café ☕".as_bytes()), "café ☕");
    }
}
