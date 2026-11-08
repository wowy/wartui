use wartui_bridge::BridgeInfo;
use wartui_proto::bus::{Compat, NodeBoard, RecordKind, Slot};
use wartui_proto::fan::FanStats;
use wartui_proto::link::PowerOutcome;
use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, Plan};

#[cfg(doc)]
use super::rx::DUPLICATE_BATCH_WINDOW_US;
use super::{FleetEngine, NodeState, Now, StoreStats};
use crate::gps::{Gps, GpsStatus, GpsView};
use crate::record::{Observation, ssid_text};

/// Running totals, all of them since the engine started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Node records the bridge forwarded.
    pub frames: u64,
    /// Sightings recorded.
    pub observations: u64,
    /// Records that parsed as heartbeats.
    pub heartbeats: u64,
    /// Records of an unknown type, or that would not parse.
    pub undecodable: u64,
    /// Records in a layout this build does not read: a wrong length, or a sighting batch from a
    /// node once its heartbeat has named another major or minor. A half-flashed fleet looks like
    /// this, and nothing else says so. A heartbeat naming another major or minor is not counted:
    /// its node is shown instead.
    pub incompatible: u64,
    /// Sighting batches from a slot no heartbeat has named yet, dropped: there is no node to
    /// file them under. A node's first seconds after it is plugged in.
    pub unknown_slot: u64,
    /// USB frames that failed their checksum.
    pub garbled: u64,
    /// Assignments this host has sent.
    pub admin_sent: u64,
    /// Assignments the bridge delivered.
    pub admin_acked: u64,
    /// Assignments undelivered, or never answered for.
    pub admin_failed: u64,
    /// Assignments delivered but not taken, as the node's next heartbeat epoch shows. Each is
    /// re-sent on that heartbeat, like an undelivered one.
    pub admin_unadopted: u64,
    /// Times the pool was re-partitioned.
    pub replans: u64,
    /// Sighting batches lost between nodes and the host, from `seq` gaps after a live batch.
    /// Records the bridge evicted with no host reading are its `dropped_tx`.
    pub batches_lost: u64,
    /// Sighting batches dropped as resends of the batch before ([`DUPLICATE_BATCH_WINDOW_US`]).
    /// Not in `batches_lost`, since nothing was lost.
    pub duplicate_batches: u64,
    /// Access points heard with no pending-buffer room, once per dwell. Mostly delay, not loss: an
    /// address turned away is not in the dedup ring, so the next dwell reports it. Steady growth
    /// means the buffer is too small for the area.
    pub wifi_refused: u64,
    /// The same for advertisers and the Bluetooth scan's buffer, once per scan.
    pub ble_refused: u64,
}

/// A node as of one snapshot.
///
/// Both flags are carried because the UI cannot re-derive them. A node can stream observations yet
/// take no assignment, since only a heartbeat is answered with one. And the liveness timeout is
/// engine configuration the snapshot does not carry.
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
    /// The channel pool the bridge reports, which the planner deals. `None` before its first
    /// `Status`.
    pub pool: Option<ChannelPool>,
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
    /// Nodes heartbeating inside the topology timeout. Can exceed [`Self::assignable`]: a node
    /// with no declared radio is alive but undrivable.
    pub alive: usize,
    /// How many can take an assignment: exactly the set the planner partitions.
    pub assignable: usize,
    /// Nodes heard inside the topology timeout whose heartbeat named an older major or minor than
    /// this build's. Never [`Self::alive`]: their heartbeats are not liveness.
    pub needs_update: usize,
    /// The same for a newer major or minor: the host is the one to update.
    pub host_behind: usize,
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
    /// What the GPS is doing, when one is read: a host receiver pinned with `--gps`, else the
    /// bridge's receiver while it is sending sentences, else the host's unless its search found
    /// nothing, else the bridge's while a bridge is connected. [`GpsStatus::NoReceiver`] when
    /// neither end has a receiver. Separate from [`Self::position`]: "no fix" and "no receiver"
    /// look identical in a row but are different problems.
    pub gps: Option<GpsView>,
    /// The SSIDs never captured, as text for the settings modal to edit.
    pub excluded_ssids: Vec<String>,
    /// The bridge's last answer to [`Command::PowerCycle`](super::Command::PowerCycle), for the
    /// view to show once.
    pub power_answer: Option<PowerAnswer>,
}

/// The bridge's answer to a power-cycle, as the operator reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerAnswer {
    /// Counts the answers this engine has had, from 1, so the view tells a new one from the last.
    pub id: u64,
    /// What happened, in [`power_cycled_text`]'s words.
    pub text: String,
}

/// Why a bridge with no carrier board cannot power-cycle a node, as the operator reads it.
pub const NO_CARRIER: &str = "this bridge has no carrier board, so it cannot switch node power";

/// What the operator reads of a power-cycle's `outcome` for `slot` on `board`.
#[must_use]
pub fn power_cycled_text(board: NodeBoard, slot: Slot, outcome: PowerOutcome) -> String {
    let name = board.label(slot);
    match outcome {
        PowerOutcome::Done => format!("{name} power-cycled"),
        PowerOutcome::NoCarrier => NO_CARRIER.to_owned(),
        PowerOutcome::NotDriven => format!("{name} is not a slot on this {board}"),
        PowerOutcome::Failed => {
            format!("{name} was not power-cycled: the carrier's I/O expander did not answer")
        }
    }
}

/// What carries the nodes, as `bridge` announced it: the bench before any bridge has.
pub(super) fn announced_board(bridge: Option<&BridgeInfo>) -> NodeBoard {
    bridge.map_or(NodeBoard::Bench, |bridge| bridge.node_board)
}

impl Snapshot {
    /// What carries the nodes, as the bridge last announced it: the bench before any has.
    #[must_use]
    pub fn node_board(&self) -> NodeBoard {
        announced_board(self.bridge.as_ref())
    }

    /// The nodes heard on another major or minor, as the header names them:
    /// `2 need update`, `1 host behind`, or `3 undrivable` for both. `None` when there are none.
    #[must_use]
    pub fn undrivable_text(&self) -> Option<String> {
        match (self.needs_update, self.host_behind) {
            (0, 0) => None,
            (1, 0) => Some("1 needs update".to_owned()),
            (n, 0) => Some(format!("{n} need update")),
            (0, n) => Some(format!("{n} host behind")),
            (older, newer) => Some(format!("{} undrivable", older + newer)),
        }
    }
}

/// The bridge's self-report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStatus {
    /// Node records forwarded since boot.
    pub rx_count: u32,
    /// Frames the bridge's outbound ring dropped since boot. Mostly history: a bridge powered with
    /// nothing attached drops all it hears.
    pub dropped_tx: u32,
    /// Frames dropped since this host attached: data this capture lost.
    pub dropped_since_attach: u32,
    /// Bridge uptime.
    pub uptime_ms: u32,
    /// The carrier's lid fan, or `None` with no carrier.
    pub fan: Option<FanStats>,
}

impl FleetEngine {
    /// The receiver the view reports on, which is the one positioning the rows when any is. A host
    /// receiver pinned with `--gps` first, as the chain resolves it; else the bridge's while a
    /// bridge is connected and its receiver is sending sentences, the rule the host's search
    /// stands down by (`runtime::stand_host_down`); else the host's, unless its search found
    /// nothing; else the bridge's while a bridge is connected, so a bridge-only run shows it
    /// connecting.
    ///
    /// The bridge's receiver shown silent reads [`GpsStatus::NoReceiver`] once the bridge says none
    /// answers its port, so the operator learns that neither end has one.
    fn shown_gps(&self, now_ms: i64) -> Option<GpsView> {
        let position = &self.config.position;
        let host = position.gps();
        if let Some(pinned) = host.filter(|gps| gps.is_pinned()) {
            return Some(pinned.view());
        }
        let Some(bridge) = position.bridge_gps().filter(|_| self.bridge_connected()) else {
            return host.map(Gps::view);
        };
        if bridge.is_talking(now_ms) {
            return Some(bridge.view());
        }
        let host = host.map(Gps::view).filter(|view| view.status != GpsStatus::NoReceiver);
        Some(host.unwrap_or_else(|| {
            let mut view = bridge.view();
            if self.bridge_lacks_gps {
                view.status = GpsStatus::NoReceiver;
            }
            view
        }))
    }

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
        let undrivable = |wanted: Compat| {
            nodes
                .iter()
                .filter(|n| {
                    now.mono.duration_since(n.state.last_seen) < self.config.topology_timeout
                })
                .filter(|n| {
                    n.state.capabilities.map(|caps| caps.version.compat_with_host()) == Some(wanted)
                })
                .count()
        };
        let needs_update = undrivable(Compat::NodeBehind);
        let host_behind = undrivable(Compat::NodeAhead);
        Snapshot {
            bridge: self.bridge.clone(),
            link_up: self.link_up,
            link_error: self.link_error.clone(),
            pool: self.pool,
            plan: self.plan,
            ble_node: self.ble_node,
            preferred_ble: self.preferred_ble,
            remember_ble: self.remember_ble,
            nodes,
            alive,
            assignable,
            needs_update,
            host_behind,
            tail: self.tail.iter().cloned().collect(),
            unique_wifi_aps: self.unique_wifi.estimate(),
            unique_ble_aps: self.unique_ble.estimate(),
            counters: self.counters,
            store,
            bridge_status: self.bridge_status,
            started_at_ms: self.started_at_ms,
            now_ms: now.unix_ms,
            position: self.config.position.resolve(now.unix_ms),
            gps: self.shown_gps(now.unix_ms),
            excluded_ssids: self
                .config
                .excluded_ssids
                .iter()
                .map(|ssid| String::from_utf8_lossy(ssid).into_owned())
                .collect(),
            power_answer: self.power_answer.clone(),
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use wartui_bridge::{BridgeInfo, LinkEvent};
    use wartui_proto::bus::NodeBoard;
    use wartui_proto::link::{BridgeToHost, LogStr, LoopPhase, NO_GPS_RECEIVER};
    use wartui_proto::reset::ResetCause;

    use super::super::{EngineConfig, Event};
    use super::*;
    use crate::position::{DEFAULT_MAX_AGE, PositionChain};

    /// An engine whose chain holds `host` and the bridge's receiver, connected to a bridge that
    /// has said no receiver answers its port.
    fn engine_without_bridge_receiver(host: &Gps, start: Instant) -> FleetEngine {
        let position = PositionChain::empty()
            .with_gps(host.clone())
            .with_bridge_gps(Gps::bridged(DEFAULT_MAX_AGE));
        let now = Now { mono: start, unix_ms: 0 };
        let mut engine = FleetEngine::new(EngineConfig { position, ..Default::default() }, now);
        let info = BridgeInfo {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
            fw_version: "0.1.0".to_owned(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms: 1_000,
            node_board: NodeBoard::Bench,
            radio: None,
            image: None,
        };
        engine.handle(Event::Link(LinkEvent::Connected(info)), now);
        let message = LogStr::try_from(NO_GPS_RECEIVER).expect("the text fits a LogStr");
        engine.handle(Event::Link(LinkEvent::Message(BridgeToHost::Error { message })), now);
        engine
    }

    /// The receiver the engine's snapshot reports on, a second after `start`.
    fn shown(engine: &FleetEngine, start: Instant) -> GpsView {
        let now = Now { mono: start + Duration::from_secs(1), unix_ms: 1_000 };
        engine.snapshot(now, StoreStats::default()).gps.expect("a receiver")
    }

    #[test]
    fn engine_snapshot_shows_no_receiver_when_host_search_and_bridge_found_none() {
        let start = Instant::now();
        let host = Gps::detached();
        host.set_status(GpsStatus::NoReceiver);
        let gps = shown(&engine_without_bridge_receiver(&host, start), start);
        assert_eq!(gps.status, GpsStatus::NoReceiver);
        assert!(gps.via_bridge, "the host's search found nothing, so the bridge's is shown");
    }

    #[test]
    fn engine_snapshot_shows_host_status_when_host_is_scanning_and_bridge_found_none() {
        let start = Instant::now();
        let host = Gps::detached();
        let scanning = GpsStatus::Scanning { port: "/dev/ttyUSB0".to_owned(), baud: 9_600 };
        host.set_status(scanning.clone());
        let engine = engine_without_bridge_receiver(&host, start);
        assert_eq!(shown(&engine, start).status, scanning);
    }

    #[test]
    fn engine_forgets_missing_bridge_receiver_when_bridge_disconnects() {
        let start = Instant::now();
        let mut engine = engine_without_bridge_receiver(&Gps::detached(), start);
        assert!(engine.bridge_lacks_gps);
        let now = Now { mono: start, unix_ms: 0 };
        let reason = "unplugged".to_owned();
        engine.handle(Event::Link(LinkEvent::Disconnected { reason }), now);
        assert!(!engine.bridge_lacks_gps);
    }
}
