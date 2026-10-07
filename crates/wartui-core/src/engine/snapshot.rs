use wartui_bridge::BridgeInfo;
use wartui_proto::air::RecordKind;
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, Plan};

#[cfg(doc)]
use super::rx::DUPLICATE_BATCH_WINDOW_US;
use super::{FleetEngine, NodeState, Now, StoreStats};
use crate::record::{Observation, ssid_text};

/// Running totals, all of them since the engine started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// ESP-NOW frames the bridge forwarded.
    pub frames: u64,
    /// Frames that parsed as observations.
    pub observations: u64,
    /// Frames that parsed as heartbeats.
    pub heartbeats: u64,
    /// Frames that were not ours and were not the vendor's either.
    pub undecodable: u64,
    /// Frames of ours speaking another wire version. A half-flashed fleet looks like this, and
    /// nothing else says so.
    pub incompatible: u64,
    /// Vendor heartbeats and observations: another fleet is transmitting on this channel.
    pub foreign_fleet: u64,
    /// Assignments this host did not send: another core is driving a fleet nearby.
    pub foreign_admin: u64,
    /// USB frames that failed their checksum.
    pub garbled: u64,
    /// Assignments held back because their heartbeat was replayed from the bridge's backlog, its
    /// window long shut. Counted only where one was owed, so this is transmits deferred, not stale
    /// frames.
    pub admin_windows_missed: u64,
    /// Assignments this host has put on the air.
    pub admin_sent: u64,
    /// Assignments a node's radio acknowledged.
    pub admin_acked: u64,
    /// Assignments unacknowledged, refused by the bridge, or never answered for.
    pub admin_failed: u64,
    /// Assignments acked but not taken, as the node's next heartbeat epoch shows. Each is re-sent
    /// on that heartbeat, like an unacknowledged one.
    pub admin_unadopted: u64,
    /// Assignments refused because the bridge's peer table was full.
    pub peer_table_full: u64,
    /// Times the pool was re-partitioned.
    pub replans: u64,
    /// Sighting batches lost between nodes and the host, from `seq` gaps after a live batch. Frames
    /// the bridge evicted with no host reading are its `dropped_tx`.
    pub batches_lost: u64,
    /// Sighting batches dropped as MAC-layer retransmissions ([`DUPLICATE_BATCH_WINDOW_US`]). Not
    /// in `batches_lost`, since nothing was lost.
    pub duplicate_batches: u64,
    /// Access points heard with no pending-buffer room, once per dwell. Mostly delay, not loss: an
    /// address turned away is not in the dedup ring, so the next dwell reports it. Steady growth
    /// means the buffer is too small for the area.
    pub wifi_dropped: u64,
    /// The same for advertisers and the Bluetooth scan's buffer, once per scan.
    pub ble_dropped: u64,
}

/// A node as of one snapshot.
///
/// Both flags are carried because the UI cannot re-derive them. A node can stream observations with
/// a healthy RSSI yet take no assignment, since only heartbeats open its admin window. And the
/// liveness timeout is engine configuration the snapshot does not carry.
#[derive(Debug, Clone)]
pub struct NodeView {
    /// Everything known about the node.
    pub state: NodeState,
    /// Whether it has heartbeated inside the topology timeout.
    pub alive: bool,
    /// Whether it can take an assignment ([`FleetEngine::is_assignable`]).
    pub assignable: bool,
}

/// One row of the UI's observation stream.
#[derive(Debug, Clone, PartialEq)]
pub struct TailEntry {
    /// Which node reported it.
    pub node_mac: Mac,
    /// Unix milliseconds of receipt.
    pub rx_at_ms: i64,
    /// The observed BSSID.
    pub bssid: [u8; 6],
    /// SSID as [`ssid_text`] renders it, for human eyes. The store keeps the bytes. Empty means
    /// hidden, which the view shows, so padding must be gone by here.
    pub ssid: String,
    /// The `AuthMode` token.
    pub security: String,
    /// Channel, or 0 for BLE.
    pub channel: u16,
    /// Signal strength as the node measured it.
    pub rssi: i16,
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
}

/// What the UI renders. Rebuilt on a tick, never streamed row by row.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The bridge, once it has announced itself.
    pub bridge: Option<BridgeInfo>,
    /// Whether the link is currently up.
    pub link_up: bool,
    /// Why it went down, if it did.
    pub link_error: Option<String>,
    /// The configured channel pool.
    pub pool: ChannelPool,
    /// The partition in force. `None` with nothing heartbeating, or more nodes alive than wartui
    /// supports.
    pub plan: Option<Plan>,
    /// Which node is scanning Bluetooth, if any.
    pub ble_node: Option<Mac>,
    /// The node the scan returns to whenever it is assignable and none holds it.
    pub preferred_ble: Option<Mac>,
    /// Whether giving a node the scan records it as [`Self::preferred_ble`].
    pub remember_ble: bool,
    /// Every node, ordered by MAC so the table does not reshuffle itself.
    pub nodes: Vec<NodeView>,
    /// Nodes heartbeating inside the topology timeout. Can exceed [`Self::assignable`]: a node with
    /// no peer slot, or no declared radio, is alive but undrivable.
    pub alive: usize,
    /// How many can take an assignment: exactly the set the planner partitions.
    pub assignable: usize,
    /// The most recent observations, newest last.
    pub tail: Vec<TailEntry>,
    /// Distinct Wi-Fi BSSIDs this session, estimated to about 0.8% standard error
    /// ([`crate::distinct`]).
    pub unique_wifi_aps: u64,
    /// Distinct BLE addresses seen this session, estimated the same way.
    pub unique_ble_aps: u64,
    /// Engine totals.
    pub counters: Counters,
    /// Store totals.
    pub store: StoreStats,
    /// The bridge's own last reported counters.
    pub bridge_status: Option<BridgeStatus>,
    /// Unix milliseconds the session began.
    pub started_at_ms: i64,
    /// Unix milliseconds this snapshot was taken.
    pub now_ms: i64,
    /// Where the host believes it is, resolved as of this snapshot.
    pub position: crate::position::Fix,
    /// What the GPS is doing, when one is configured. Separate from [`Self::position`]: "no fix"
    /// and "no receiver" look identical in a row but are different problems.
    pub gps: Option<crate::gps::GpsView>,
    /// The nodes' Wi-Fi transmit power, in ESP-IDF quarter-dBm units, for the settings modal.
    pub tx_power: i8,
    /// The bridge's Wi-Fi transmit power, in ESP-IDF quarter-dBm units, for the settings modal.
    pub bridge_tx_power: i8,
}

/// The bridge's self-report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStatus {
    /// Registered peers.
    pub peer_count: u8,
    /// Frames received since boot.
    pub rx_count: u32,
    /// Frames the bridge's outbound ring dropped since boot. Mostly history: a bridge powered with
    /// nothing attached drops all it hears.
    pub dropped_tx: u32,
    /// Frames dropped since this host attached: data this capture lost.
    pub dropped_since_attach: u32,
    /// Bridge uptime.
    pub uptime_ms: u32,
}

impl FleetEngine {
    /// Build the view the UI renders. Taken per tick, not per event, so an observation burst is not
    /// a redraw burst.
    #[must_use]
    pub fn snapshot(&self, now: Now, store: StoreStats) -> Snapshot {
        let nodes: Vec<NodeView> = self
            .nodes
            .values()
            .map(|state| NodeView {
                alive: self.is_alive(state, now),
                assignable: self.is_assignable(state, now),
                state: state.clone(),
            })
            .collect();
        let alive = nodes.iter().filter(|n| n.alive).count();
        let assignable = nodes.iter().filter(|n| n.assignable).count();
        Snapshot {
            bridge: self.bridge.clone(),
            link_up: self.link_up,
            link_error: self.link_error.clone(),
            pool: self.config.pool,
            plan: self.plan,
            ble_node: self.ble_node,
            preferred_ble: self.preferred_ble,
            remember_ble: self.remember_ble,
            nodes,
            alive,
            assignable,
            tail: self.tail.iter().cloned().collect(),
            unique_wifi_aps: self.unique_wifi.estimate(),
            unique_ble_aps: self.unique_ble.estimate(),
            counters: self.counters,
            store,
            bridge_status: self.bridge_status,
            started_at_ms: self.started_at_ms,
            now_ms: now.unix_ms,
            position: self.config.position.resolve(now.unix_ms),
            gps: self.config.position.gps().map(crate::gps::Gps::view),
            tx_power: self.config.tx_power,
            bridge_tx_power: self.config.bridge_tx_power,
        }
    }

    pub(super) fn push_tail(&mut self, observation: &Observation) {
        if self.config.tail_len == 0 {
            return;
        }
        while self.tail.len() >= self.config.tail_len {
            self.tail.pop_front();
        }
        self.tail.push_back(TailEntry {
            node_mac: observation.node_mac,
            rx_at_ms: observation.rx_at_ms,
            bssid: observation.bssid,
            ssid: ssid_text(&observation.ssid),
            security: observation.security.clone(),
            channel: observation.channel,
            rssi: observation.rssi,
            kind: observation.kind,
        });
    }
}
