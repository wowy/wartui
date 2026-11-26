//! What a front end shows of a capture: one [`View`], derived from a [`Snapshot`] by
//! [`View::of`], so a phone and the terminal show the same capture by the same rules. Which faults
//! a capture has and in what order, what state each node is in, what the planner and the receiver
//! are doing: each is decided here once, and a front end only words, colors and lays it out.
//!
//! **Plain data.** A `Snapshot` cannot cross a phone binding: it carries instants, byte arrays,
//! bit sets and protocol newtypes. Every field here is a `bool`, an integer, an `f64`, a `String`,
//! an `Option` or `Vec` of these, a type of this module, or one of five plain enums:
//! [`ChannelPool`], [`ResetCause`], [`Board`], [`PositionSource`] and [`Constellation`].
//! Milliseconds are `i64`, the engine's `u64` totals stay `u64`, and counts of nodes are `u32`.
//!
//! **Data, never sentences.** A `String` here holds a datum: an address, a slot's label, a port, a
//! version, or the transport's own account of a failure. The words an operator reads are each
//! front end's.
//!
//! **Pure.** [`View::of`] reads no clock and does no I/O: everything time-dependent comes from
//! [`Snapshot::now_ms`].

mod faults;
mod nodes;
mod receiver;

use wartui_core::engine::Snapshot;
#[cfg(doc)]
use wartui_core::nmea::Constellation;
use wartui_core::position::PositionSource;
#[cfg(doc)]
use wartui_proto::bus::Board;
use wartui_proto::fleet::ScanMode;
use wartui_proto::mac;
use wartui_proto::plan::{ChannelPool, SCAN_CHANNELS};
#[cfg(doc)]
use wartui_proto::reset::ResetCause;

pub use faults::{Fault, LinkFault, LinkFaultKind, Note, RejectsFrom};
pub use nodes::{FirmwareShown, NodeRow, NodeStatus, PowerCycle, Severity, ShareShown, SlotShown};
pub use receiver::{
    Augmentation, FixShown, ReceiverPort, ReceiverStatus, ReceiverSummary, ReceiverView,
    SatelliteShown, SkyDate, SkyView,
};

use crate::settings::Link;

/// Everything a front end's fleet view shows of one snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// Unix milliseconds the snapshot was taken.
    pub now_ms: i64,
    /// Unix milliseconds the session began.
    pub started_at_ms: i64,
    /// The host link and its state.
    pub link: LinkView,
    /// The bridge, once it has announced itself.
    pub bridge: Option<BridgeView>,
    /// The carrier's lid fan as the bridge last reported it.
    pub fan: Option<FanShown>,
    /// What the bridge's planner is doing.
    pub fleet: FleetView,
    /// Every node, in the snapshot's order.
    pub nodes: Vec<NodeRow>,
    /// The capture's running totals.
    pub totals: Totals,
    /// Wi-Fi sightings per channel of the bridge's pool, in scan-table order.
    pub chart: Vec<ChannelBar>,
    /// Whether the rows have a position, and where it comes from.
    pub position: PositionView,
    /// The GPS receiver the view reports on, when one is read.
    pub receiver: Option<ReceiverView>,
    /// What the operator should know that is no fault, in the order a front end lists them.
    pub notes: Vec<Note>,
    /// Every fault so far, most urgent first.
    pub faults: Vec<Fault>,
    /// The run holds its scan for a GPS fix.
    pub awaiting_fix: bool,
}

impl View {
    /// What a front end shows of `snapshot`, for a run that reaches its bridge over `over`.
    #[must_use]
    pub fn of(snapshot: &Snapshot, over: Link) -> Self {
        let nodes: Vec<NodeRow> =
            snapshot.nodes.iter().map(|node| NodeRow::of(snapshot, node)).collect();
        let (notes, faults) = faults::of(snapshot, &nodes);
        Self {
            now_ms: snapshot.now_ms,
            started_at_ms: snapshot.started_at_ms,
            link: LinkView { up: snapshot.link_up, host: host(snapshot, over) },
            bridge: snapshot.bridge.as_ref().map(|bridge| BridgeView {
                mac: mac::full(&bridge.mac).to_string(),
                mac_tail: mac::short(&bridge.mac).to_string(),
                firmware: bridge.fw_version.clone(),
            }),
            fan: fan(snapshot),
            fleet: FleetView {
                pool: snapshot.pool,
                scan: scan(snapshot),
                planning: planning(snapshot),
            },
            nodes,
            totals: totals(snapshot),
            chart: chart(snapshot),
            position: PositionView {
                located: snapshot.position.is_located(),
                source: snapshot.position.source,
            },
            receiver: snapshot.gps.as_ref().map(|gps| receiver::of(snapshot, gps)),
            notes,
            faults,
            awaiting_fix: snapshot.awaiting_fix,
        }
    }
}

/// The host link and whether it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkView {
    /// The link is up.
    pub up: bool,
    /// The host link the run uses, and what the bridge says of it.
    pub host: HostShown,
}

/// The host link the run uses, and what the bridge's latest `Status` says of it while the link is
/// up: a stale one would describe a host that has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostShown {
    /// Over USB. `bluetooth_waits`: a host the bridge would serve is connected to its C6.
    Usb {
        /// An encrypted, bonded central is connected to the C6, and waits while USB holds the
        /// bridge.
        bluetooth_waits: bool,
    },
    /// Over Bluetooth, with the link's MTU once the bridge has reported it.
    Bluetooth {
        /// The ATT MTU the bridge reports. `None` before it has.
        mtu: Option<u16>,
    },
}

/// The bridge, as it announced itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeView {
    /// Its whole address.
    pub mac: String,
    /// The last two octets of its address, which tell boards apart.
    pub mac_tail: String,
    /// Its firmware version, `0.7.0`.
    pub firmware: String,
}

/// The carrier's lid fan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanShown {
    /// The duty in force, in percent.
    pub duty_pct: u8,
    /// The speed over the last whole tach window. `None` before one.
    pub rpm: Option<u16>,
}

/// What the bridge's planner is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetView {
    /// The channel pool the bridge reports. `None` before its first `Status`.
    pub pool: Option<ChannelPool>,
    /// What the planner is asked to scan.
    pub scan: ScanShown,
    /// How many nodes hold a share, or why none does.
    pub planning: Planning,
}

/// What the bridge's planner scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanShown {
    /// The bridge has not reported yet.
    Unknown,
    /// Every node idles.
    Stopped,
    /// Every node sniffs Wi-Fi.
    Wifi,
    /// Wi-Fi, with Bluetooth asked for. `node`: the label of the slot given the Bluetooth scan, or
    /// `None` while no node holds it.
    WifiBluetooth {
        /// The label of the slot the bridge gave the Bluetooth scan.
        node: Option<String>,
    },
}

/// How many nodes hold a share of the bridge's plan, or why none does, first match first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planning {
    /// `held` of the `of` nodes heard hold a share.
    Shares {
        /// Nodes holding a share.
        held: u32,
        /// Nodes heard.
        of: u32,
    },
    /// Nodes heartbeat, and none is assignable.
    NoneUsable {
        /// Nodes heartbeating.
        alive: u32,
    },
    /// No node heard yet.
    WaitingForNodes,
    /// Nodes heard only on an older major or minor.
    NeedUpdate {
        /// How many.
        nodes: u32,
    },
    /// Nodes heard only on a newer major or minor: the host is the one to update.
    HostBehind {
        /// How many.
        nodes: u32,
    },
    /// Nodes heard on older and on newer majors or minors.
    Undrivable {
        /// How many, both kinds together.
        nodes: u32,
    },
    /// Every node heard has stopped heartbeating.
    AllSilent,
}

/// The capture's running totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Node records the bridge forwarded.
    pub frames: u64,
    /// Sightings recorded.
    pub sightings: u64,
    /// Of them, Wi-Fi.
    pub wifi_sightings: u64,
    /// Of them, Bluetooth.
    pub bluetooth_sightings: u64,
    /// Heartbeats.
    pub heartbeats: u64,
    /// Rows the store wrote.
    pub stored: u64,
    /// Sighting batches lost between the nodes and the host.
    pub batches_lost: u64,
    /// Sighting batches dropped as resends.
    pub duplicate_batches: u64,
    /// Distinct Wi-Fi access points, estimated.
    pub unique_wifi: u64,
    /// Distinct Bluetooth advertisers, estimated.
    pub unique_bluetooth: u64,
}

/// One charted channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelBar {
    /// The channel number.
    pub channel: u8,
    /// Wi-Fi sightings on it.
    pub sightings: u64,
    /// A band starts at it.
    pub starts_band: bool,
}

/// Whether the rows have a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionView {
    /// The rows carry a position.
    pub located: bool,
    /// Which kind of tier gave it.
    pub source: PositionSource,
}

/// The host link, reading the bridge's connected central only while the link is up.
fn host(snapshot: &Snapshot, over: Link) -> HostShown {
    let central = snapshot
        .bridge_status
        .filter(|_| snapshot.link_up)
        .and_then(|status| status.bluetooth.central);
    match over {
        Link::Usb => HostShown::Usb {
            bluetooth_waits: central.is_some_and(|central| central.encrypted && central.bonded),
        },
        Link::Bluetooth => HostShown::Bluetooth { mtu: central.map(|central| central.mtu) },
    }
}

/// The fan, or `None` with the link down, when the report is stale, before the bridge's first
/// `Status`, and with no carrier.
fn fan(snapshot: &Snapshot) -> Option<FanShown> {
    if !snapshot.link_up {
        return None;
    }
    let fan = snapshot.bridge_status?.fan?;
    Some(FanShown { duty_pct: fan.duty.get(), rpm: fan.rpm })
}

/// What the planner scans, naming the Bluetooth node by its slot's label on the announced board.
fn scan(snapshot: &Snapshot) -> ScanShown {
    match snapshot.scan {
        None => ScanShown::Unknown,
        Some(ScanMode::Stopped) => ScanShown::Stopped,
        Some(ScanMode::Wifi) => ScanShown::Wifi,
        Some(ScanMode::WifiBluetooth) => ScanShown::WifiBluetooth {
            node: snapshot.ble_slot.map(|slot| snapshot.node_board().label(slot).to_string()),
        },
    }
}

/// A count of nodes, as the view carries it.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Shares held; else nodes alive and none assignable; else no node heard; else nodes on another
/// major or minor; else silence.
fn planning(snapshot: &Snapshot) -> Planning {
    let held = snapshot.nodes.iter().filter(|node| node.share.is_some()).count();
    if held > 0 {
        return Planning::Shares { held: count(held), of: count(snapshot.nodes.len()) };
    }
    if snapshot.alive > 0 {
        return Planning::NoneUsable { alive: count(snapshot.alive) };
    }
    if snapshot.nodes.is_empty() {
        return Planning::WaitingForNodes;
    }
    match (snapshot.needs_update, snapshot.host_behind) {
        (0, 0) => Planning::AllSilent,
        (older, 0) => Planning::NeedUpdate { nodes: count(older) },
        (0, newer) => Planning::HostBehind { nodes: count(newer) },
        (older, newer) => Planning::Undrivable { nodes: count(older + newer) },
    }
}

fn totals(snapshot: &Snapshot) -> Totals {
    let c = &snapshot.counters;
    Totals {
        frames: c.frames,
        sightings: c.observations,
        wifi_sightings: c.wifi_observations,
        bluetooth_sightings: c.ble_observations,
        heartbeats: c.heartbeats,
        stored: snapshot.store.written,
        batches_lost: c.batches_lost,
        duplicate_batches: c.duplicate_batches,
        unique_wifi: snapshot.unique_wifi_aps,
        unique_bluetooth: snapshot.unique_ble_aps,
    }
}

/// The channels each band starts at: 2.4 GHz, then the three 5 GHz runs the table holds.
const BAND_STARTS: [u8; 4] = [1, 36, 100, 149];

/// One bar per channel of the bridge's pool, in table order, or of `All` before the bridge reports
/// one: the pool a bridge with nothing stored holds. A channel outside the pool has no bar,
/// whatever it counts, and channel 14 is in no pool.
fn chart(snapshot: &Snapshot) -> Vec<ChannelBar> {
    let pool = snapshot.pool.unwrap_or(ChannelPool::All);
    (0u8..)
        .zip(SCAN_CHANNELS.iter().zip(&snapshot.wifi_by_channel))
        .filter(|&(i, _)| pool.contains(i))
        .map(|(_, (&channel, &sightings))| ChannelBar {
            channel,
            sightings,
            starts_band: BAND_STARTS.contains(&channel),
        })
        .collect()
}
