//! What the engine hands to the store.
//!
//! Plain owned data with no database types in it, so the engine stays free of
//! `rusqlite` and can be tested without one. The store's job is to turn these
//! into rows; deciding what is worth recording is the engine's.
//!
//! [`ssid_text`] lives here rather than beside either caller because both read this
//! module's bytes, and written out twice the two answers drifted apart.

use wartui_proto::air::RecordKind;
use wartui_proto::beacon::visible_ssid;
use wartui_proto::link::Mac;
use wartui_proto::plan::ChannelSet;

use crate::position::Fix;

/// Raw SSID bytes as text, for anything that has to show them to a person or
/// write them into a column that has to be parsed as text.
///
/// One rule in one place, because the view and the export were quietly disagreeing
/// about the same bytes. [`visible_ssid`] strips a cloaked access point's padding,
/// applied here as well as in the parser because captures recorded before that rule
/// existed are still read back through this. Any NUL left standing is an interior
/// one, and gets what `from_utf8_lossy` gives any byte it cannot represent — a raw
/// NUL is worth no more to a terminal than to WiGLE's parser.
///
/// The store keeps whatever arrived; this is only how it is read back out.
#[must_use]
pub fn ssid_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(visible_ssid(bytes));
    if text.contains('\0') { text.replace('\0', "\u{FFFD}") } else { text.into_owned() }
}

/// A node was heard from, which is enough to keep its row current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSeen {
    /// The node's full six-byte MAC.
    ///
    /// Divergence 1.
    pub mac: Mac,
    /// Unix milliseconds when this host first saw the node in this session.
    pub first_seen_ms: i64,
    /// Unix milliseconds of the most recent frame of any kind.
    pub last_seen_ms: i64,
    /// The capability token from this node's most recent heartbeat, verbatim,
    /// or `None` for a frame that carried none.
    ///
    /// Kept as the text that was on the wire rather than the parsed value, so a
    /// capture can answer "what did this node say it was" for a version this build
    /// did not understand. It is also the only record of *why* a node was never
    /// assigned anything: heartbeats, no token and no assignments is the diagnosis.
    ///
    /// `None` does not erase what an earlier heartbeat said. Most frames are
    /// observations and carry no token, so the store coalesces rather than
    /// overwriting, and the stored column is the last token *ever* seen from this
    /// node rather than the last one it sent — so a board reflashed mid-capture
    /// keeps its old token in the file while the engine stops believing it. Every
    /// other site that needs this rule points here.
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
    /// How strongly the bridge heard the reporting node, not the observed network:
    /// a node at the edge of the mesh is worth spotting in the data afterwards.
    pub link_rssi: Option<i8>,
    /// The observed BSSID, six raw bytes.
    pub bssid: [u8; 6],
    /// Raw SSID bytes. Not a `String`: an SSID is whatever the access point
    /// beaconed, and forcing it through UTF-8 here would lose the original.
    /// [`ssid_text`] is how it becomes text again, wherever something has to
    /// show it or write it down.
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
    /// The frame exactly as it came off the air, header and all.
    ///
    /// A couple of dozen bytes a row, and the reason a decoder fix can reach history
    /// rather than only what arrives afterwards. The header says which version wrote
    /// the row, so a file spanning a format change is still readable.
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
    /// Channel the bridge was parked on.
    pub channel: Option<u8>,
    /// The undecoded payload.
    pub bytes: Vec<u8>,
}

/// Which bridge this capture came through.
///
/// Separate from [`crate::store::SessionInfo`] because a session is opened before
/// any bridge has announced itself, and a capture that never finds a dongle still
/// deserves a session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSeen {
    /// The bridge's own MAC, which nodes see as the core's address.
    pub mac: Mac,
    /// Which chip it is.
    pub chip: String,
    /// Its firmware version.
    pub fw_version: String,
}

/// What became of one assignment this host put on the air.
///
/// Every attempt gets a row, successful or not, which is what turns "the fleet keeps
/// drifting off its channels" into a query. Divergence 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminOutcome {
    /// The node's radio acknowledged the frame at the MAC layer.
    Acked,
    /// It went out and nothing came back. Usually BLE coexistence on the node.
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
    /// Divergence 4.
    pub counter: u64,
    /// The byte that actually went on the wire, `air::wire_epoch(counter)`.
    /// The column keeps its older name; what it holds has not changed.
    pub wire_version: u8,
    /// The node's slot in the fleet-wide stagger order.
    pub node_index: u8,
    /// Fleet size as of the assignment, not as of the send — divergence 7.
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
    /// Bridge-measured microseconds from the heartbeat that opened the node's admin
    /// window to the transmit callback, stamped at both ends by the bridge so the
    /// host's scheduling noise never enters it.
    ///
    /// `None` when there is no such measurement to make: no heartbeat stamp yet, or
    /// a difference longer than the window, which means the heartbeat behind it did
    /// not open one. The outcome is recorded either way.
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

#[cfg(test)]
mod tests {
    use super::ssid_text;

    #[test]
    fn a_cloaked_ssid_is_stripped_back_to_a_hidden_network() {
        assert_eq!(ssid_text(&[0u8; 8]), "");
        assert_eq!(ssid_text(b"Home\0\0\0"), "Home");
        assert_eq!(ssid_text(b""), "");
        assert_eq!(ssid_text(b"plain"), "plain");
    }

    #[test]
    fn a_nul_that_is_not_padding_reaches_neither_the_file_nor_the_terminal() {
        // An interior NUL is left alone by the trim, so this is the last thing
        // standing between it and a column WiGLE has to parse, or a terminal
        // that would swallow it and show a shorter name than the one on air.
        assert_eq!(ssid_text(b"a\0b"), "a\u{FFFD}b");
    }
}
