//! What a phone shows of a capture: `wartui_app::view`'s [`View`], as the binding carries it.
//!
//! Each type here is a remote declaration of the app's own, field for field and variant for
//! variant, so the phone sees the app's shape: nothing renamed, reordered or flattened. A field or
//! variant added on one side only fails to compile. The five plain enums the view uses from the
//! crates below are declared here too.
//!
//! The view words nothing: a `String` is a datum (an address, a label, a port) or the transport's
//! own account of a failure. The phone words the rest.

use wartui_app::view::{
    Augmentation, BridgeView, ChannelBar, FanShown, Fault, FirmwareShown, FixShown, FleetView,
    HostShown, LinkFault, LinkFaultKind, LinkView, NodeRow, NodeStatus, Note, Planning,
    PositionView, PowerCycle, ReceiverPort, ReceiverStatus, ReceiverSummary, ReceiverView,
    RejectsFrom, SatelliteShown, ScanShown, Severity, ShareShown, SkyDate, SkyView, SlotShown,
    Totals, View,
};
use wartui_core::nmea::Constellation;
use wartui_core::position::PositionSource;
use wartui_proto::bus::Board;
use wartui_proto::plan::ChannelPool;
use wartui_proto::reset::ResetCause;

/// A channel pool: the channels the bridge's planner deals.
#[uniffi::remote(Enum)]
pub enum ChannelPool {
    /// FCC-permitted channels: 2.4 GHz 1-11 and 5 GHz 36-165.
    Us,
    /// ETSI-permitted channels: 2.4 GHz 1-13 and 5 GHz 36-140.
    Eu,
    /// Every channel a node can tune: 2.4 GHz 1-13 and all of 5 GHz.
    All,
}

/// Why a board is running this life rather than the last one.
#[uniffi::remote(Enum)]
pub enum ResetCause {
    /// Plugged in, or the button was pressed.
    PowerOn,
    /// The firmware reset itself.
    Software,
    /// A watchdog fired.
    Watchdog,
    /// The CPU locked up.
    Lockup,
    /// The supply sagged.
    Brownout,
    /// A reset the firmware cannot attribute.
    External,
    /// A cause this build has no name for.
    Unknown,
}

/// The board a node's heartbeats announce.
#[uniffi::remote(Enum)]
pub enum Board {
    /// An ESP32-C5 board.
    Esp32c5,
    /// The ESP32-C6 that runs a P4 bridge's radio.
    Esp32c6,
    /// An ESP32-P4 bridge.
    Esp32p4,
    /// A byte this build does not name.
    Unknown(u8),
}

/// Which kind of position tier placed the rows.
#[uniffi::remote(Enum)]
pub enum PositionSource {
    /// A GPS receiver's live fix, the bridge's or the host's.
    Gps,
    /// The platform's location services.
    Platform,
    /// A typed-in position.
    Static,
    /// No position.
    None,
}

/// A satellite navigation system.
#[uniffi::remote(Enum)]
pub enum Constellation {
    Gps,
    Glonass,
    Galileo,
    BeiDou,
    Qzss,
    NavIc,
}

/// Everything a fleet view shows of one snapshot.
#[uniffi::remote(Record)]
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
    /// What the operator should know that is no fault, in the order to list them.
    pub notes: Vec<Note>,
    /// Every fault so far, most urgent first.
    pub faults: Vec<Fault>,
    /// The capture holds its scan for a GPS fix.
    pub awaiting_fix: bool,
}

/// The host link and whether it is up.
#[uniffi::remote(Record)]
pub struct LinkView {
    /// The link is up.
    pub up: bool,
    /// The host link the capture uses, and what the bridge says of it.
    pub host: HostShown,
}

/// The host link the capture uses, and what the bridge's latest `Status` says of it.
#[uniffi::remote(Enum)]
pub enum HostShown {
    /// Over USB.
    Usb {
        /// A bonded central is connected to the C6, and waits while USB holds the bridge.
        bluetooth_waits: bool,
    },
    /// Over Bluetooth.
    Bluetooth {
        /// The ATT MTU the bridge reports. `None` before it has.
        mtu: Option<u16>,
    },
}

/// The bridge, as it announced itself.
#[uniffi::remote(Record)]
pub struct BridgeView {
    /// Its whole address.
    pub mac: String,
    /// The last two octets of its address, which tell boards apart.
    pub mac_tail: String,
    /// Its firmware version, `0.7.0`.
    pub firmware: String,
}

/// The carrier's lid fan.
#[uniffi::remote(Record)]
pub struct FanShown {
    /// The duty in force, in percent.
    pub duty_pct: u8,
    /// The speed over the last whole tach window. `None` before one.
    pub rpm: Option<u16>,
}

/// What the bridge's planner is doing.
#[uniffi::remote(Record)]
pub struct FleetView {
    /// The channel pool the bridge reports. `None` before its first `Status`.
    pub pool: Option<ChannelPool>,
    /// What the planner is asked to scan.
    pub scan: ScanShown,
    /// How many nodes hold a share, or why none does.
    pub planning: Planning,
}

/// What the bridge's planner scans.
#[uniffi::remote(Enum)]
pub enum ScanShown {
    /// The bridge has not reported yet.
    Unknown,
    /// Every node idles.
    Stopped,
    /// Every node sniffs Wi-Fi.
    Wifi,
    /// Wi-Fi, with Bluetooth asked for.
    WifiBluetooth {
        /// The label of the slot given the Bluetooth scan, or `None` while no node holds it.
        node: Option<String>,
    },
}

/// How many nodes hold a share of the bridge's plan, or why none does.
#[uniffi::remote(Enum)]
pub enum Planning {
    /// `held` of the `of` nodes heard hold a share.
    Shares { held: u32, of: u32 },
    /// Nodes heartbeat, and none is assignable.
    NoneUsable { alive: u32 },
    /// No node heard yet.
    WaitingForNodes,
    /// Nodes heard only on an older major or minor.
    NeedUpdate { nodes: u32 },
    /// Nodes heard only on a newer major or minor: the host is the one to update.
    HostBehind { nodes: u32 },
    /// Nodes heard on older and on newer majors or minors.
    Undrivable { nodes: u32 },
    /// Every node heard has stopped heartbeating.
    AllSilent,
}

/// The capture's running totals.
#[uniffi::remote(Record)]
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
#[uniffi::remote(Record)]
pub struct ChannelBar {
    /// The channel number.
    pub channel: u8,
    /// Wi-Fi sightings on it.
    pub sightings: u64,
    /// A band starts at it.
    pub starts_band: bool,
}

/// Whether the rows have a position.
#[uniffi::remote(Record)]
pub struct PositionView {
    /// The rows carry a position.
    pub located: bool,
    /// Which kind of tier gave it.
    pub source: PositionSource,
}

/// Something the operator should know that is no fault.
#[uniffi::remote(Enum)]
pub enum Note {
    /// Sightings a full pending buffer turned away. At least one count is non-zero.
    BufferFull { wifi: u64, bluetooth: u64 },
    /// The link is down because a USB host took the bridge from this Bluetooth host.
    UsbHoldsBridge,
    /// The link waits on the OS's pairing. `notice` is the transport's.
    WaitingOnPairing { notice: String },
    /// The subscription is through and the bridge has not answered. `notice` is the
    /// transport's.
    WaitingOnReady { notice: String },
}

/// Something wrong with the capture, the bridge, the receiver or a node.
#[uniffi::remote(Enum)]
pub enum Fault {
    /// The link is down.
    LinkDown(LinkFault),
    /// The store fell behind and dropped rows.
    StoreDropped { rows: u64 },
    /// Channels of the pool in no share of the bridge's plan, as channel numbers.
    Unswept { pool: ChannelPool, channels: Vec<u8> },
    /// The GPS receiver's port could not be opened or stopped reading.
    GpsUnreadable { reason: String },
    /// The receiver sends lines that fail their checksum, and no fix.
    GpsRejects { lines: u64, from: RejectsFrom },
    /// Node records of an unknown type, or that would not parse.
    Undecodable { frames: u64 },
    /// Link frames that failed their checksum.
    Garbled { frames: u64 },
    /// Node records in a layout this build does not read.
    Incompatible { frames: u64 },
    /// Frames the bridge dropped since this host attached.
    BridgeDropped { frames: u64 },
    /// The bridge restarted under the capture.
    BridgeRestarted { cause: ResetCause },
    /// No tach pulses at a duty the fan turns at.
    FanStopped { duty_pct: u8 },
    /// A node restarted from a fault. `node` is its name: its slot's label, or its address's
    /// tail with no slot.
    NodeRestarted { node: String, cause: ResetCause, times: u32 },
}

/// Why the link is down.
#[uniffi::remote(Record)]
pub struct LinkFault {
    /// What kind of failure the transport names.
    pub kind: LinkFaultKind,
    /// The transport's own account of it.
    pub detail: String,
}

/// What the transport tells apart of a link failure.
#[uniffi::remote(Enum)]
pub enum LinkFaultKind {
    /// The bridge refused this host: it holds no bond for it.
    NotBonded,
    /// The bridge dropped this host before serving it, as it drops one it holds no bond for.
    MaybeNotBonded,
    /// The OS's Bluetooth is turned off.
    RadioOff,
    /// The OS does not let this app use Bluetooth.
    NotAuthorized,
    /// The OS holds a bond the bridge no longer does.
    StaleBond,
    /// The search heard no bridge it could use.
    NotAdvertising,
    /// The search heard several bridges and none was named.
    SeveralAdvertising,
    /// Nothing the transport names.
    Other,
}

/// Where a receiver's rejected lines come from.
#[uniffi::remote(Enum)]
pub enum RejectsFrom {
    /// The bridge's receiver.
    Bridge,
    /// A host receiver at the rate the operator set.
    PinnedBaud,
    /// A host receiver at the port and rate the search settled on.
    Port { port: String, baud: u32 },
    /// A host receiver the search has not settled on.
    Unknown,
}

/// One node, as of the snapshot.
#[uniffi::remote(Record)]
pub struct NodeRow {
    /// Its whole address.
    pub mac: String,
    /// The last two octets of its address.
    pub mac_tail: String,
    /// The slot it sits in, or `None` with no slot.
    pub slot: Option<SlotShown>,
    /// The board its heartbeats announce. `None` before one.
    pub board: Option<Board>,
    /// Its firmware, once a heartbeat has said.
    pub firmware: Option<FirmwareShown>,
    /// What the operator most needs to know about it.
    pub status: NodeStatus,
    /// `status`'s severity, raised to a warning for a healthy node that is hot or has a supply
    /// drop.
    pub severity: Severity,
    /// What it scans.
    pub share: ShareShown,
    /// Sightings it reported this session.
    pub sightings: u64,
    /// Its sighting batches lost between it and the host.
    pub batches_lost: u64,
    /// Its latest die temperature, in °C. `None` before a reading.
    pub die_temp_c: Option<i16>,
    /// Its die is hot.
    pub hot: bool,
    /// Its supply rail has dropped below its baseline.
    pub supply_dropped: bool,
    /// Whether a power-cycle can be asked for, or why not.
    pub power_cycle: PowerCycle,
}

/// A node's slot.
#[uniffi::remote(Record)]
pub struct SlotShown {
    /// The slot's byte.
    pub index: u8,
    /// The slot's label on the board the bridge announced: `N7` on a carrier, `B1` with none.
    pub label: String,
}

/// A node's firmware.
#[uniffi::remote(Record)]
pub struct FirmwareShown {
    /// The version it runs, `0.7.0`.
    pub version: String,
    /// This build's node version, when the node's is older.
    pub update_to: Option<String>,
}

/// What the operator most needs to know about a node.
#[uniffi::remote(Enum)]
pub enum NodeStatus {
    /// Its heartbeat names an older major or minor.
    NeedsUpdate,
    /// Its heartbeat names a newer major or minor.
    HostBehind,
    /// It has never heartbeated, and was first heard under two heartbeat periods ago.
    Starting,
    /// Sightings arrive and no heartbeat has.
    NoHeartbeat,
    /// The bridge's `Status` shows its slot absent.
    OffBus,
    /// Another node's heartbeat took its slot.
    NoSlot,
    /// Heartbeats have stopped, or it is not yet drivable.
    Stale,
    /// It restarted under the capture.
    Rebooted { times: u32 },
    /// Nothing to do.
    Alive,
}

/// How much a node's state asks of the operator.
#[uniffi::remote(Enum)]
pub enum Severity {
    /// Nothing to do.
    Ok,
    /// Worth knowing, needing nothing.
    Notice,
    /// Out of the bridge's plan for now, a supply drop or a hot die.
    Warning,
    /// Something to fix.
    Fault,
}

/// What a node scans.
#[uniffi::remote(Enum)]
pub enum ShareShown {
    /// Wi-Fi channels, as channel numbers in scan-table order. `pending`: in the bridge's plan,
    /// not yet adopted by the node.
    Channels { channels: Vec<u8>, pending: bool },
    /// The Bluetooth scan, which holds no channels.
    Bluetooth { pending: bool },
    /// No share while the bridge's scan is stopped.
    Idle,
    /// No share while the bridge scans.
    Unassigned,
}

/// Whether a power-cycle of a node can be asked for.
#[uniffi::remote(Enum)]
pub enum PowerCycle {
    /// It can.
    Allowed,
    /// The link is down.
    NotConnected,
    /// The bridge has no carrier board to switch node power with.
    NoCarrier,
    /// The node has not heartbeated, so nothing says which slot it is in.
    NotHeard,
    /// Another node's heartbeat took its slot.
    LostSlot,
}

/// The receiver the view reports on.
#[uniffi::remote(Record)]
pub struct ReceiverView {
    /// What the receiver is doing for the capture's positions.
    pub summary: ReceiverSummary,
    /// What the receiver itself reports.
    pub status: ReceiverStatus,
    /// Where it is read from.
    pub port: ReceiverPort,
    /// Sentences that parsed.
    pub sentences: u64,
    /// Lines that failed a checksum or a field.
    pub rejected: u64,
    /// The receiver's current report, while it is being read.
    pub sky: Option<SkyView>,
}

/// What the receiver does for the capture's positions.
#[uniffi::remote(Enum)]
pub enum ReceiverSummary {
    /// The port failed.
    Failed { reason: String },
    /// It positions the rows.
    Positioning { satellites: Option<u8> },
    /// The port has not been opened yet.
    Connecting,
    /// The search listens to a port at a rate.
    Scanning { port: String, baud: u32 },
    /// Sentences arrive and the receiver has no fix.
    Searching,
    /// It had a fix, and the rows no longer take it.
    StaleFix,
    /// Neither the host nor the bridge has a receiver.
    NoReceiver,
}

/// What the receiver itself reports.
#[uniffi::remote(Enum)]
pub enum ReceiverStatus {
    /// The port has not been opened yet.
    Connecting,
    /// The search listens to a port at a rate.
    Scanning { port: String, baud: u32 },
    /// Every port was listened to and none was a receiver.
    NoReceiver,
    /// The port could not be opened or stopped reading.
    Failed { reason: String },
    /// Sentences are being read. `silent_secs`: how long since the last, once past the
    /// receiver's max age.
    Reading { fix: FixShown, silent_secs: Option<i64> },
}

/// What a receiver's sentences say of its fix.
#[uniffi::remote(Enum)]
pub enum FixShown {
    /// No fix.
    Searching,
    /// No sentence has parsed.
    WaitingForSentences,
    /// A 3D fix.
    ThreeD { augmented: Augmentation },
    /// A 2D fix: no altitude.
    TwoD { augmented: Augmentation },
    /// A fix from a receiver that sends no `GSA`.
    Unspecified { augmented: Augmentation },
}

/// How a fix is augmented, from the `GGA` fix quality.
#[uniffi::remote(Enum)]
pub enum Augmentation {
    /// Plain GPS, or a quality with no name.
    None,
    /// Differential GPS.
    Dgps,
    /// RTK, fixed.
    RtkFixed,
    /// RTK, float.
    RtkFloat,
    /// Dead reckoning.
    DeadReckoning,
}

/// Where the receiver is read from.
#[uniffi::remote(Enum)]
pub enum ReceiverPort {
    /// The bridge's receiver, over the link.
    Bridge,
    /// A host port, at the rate the search settled on.
    Host { port: String, baud: u32 },
    /// A host receiver whose search has not settled on a port.
    Unsettled,
}

/// The receiver's current report.
#[uniffi::remote(Record)]
pub struct SkyView {
    /// Satellites in the solution.
    pub used: Option<u8>,
    /// The last `RMC` date, UTC.
    pub date: Option<SkyDate>,
    /// Seconds since UTC midnight of the last `GGA` or `RMC`, fix or not.
    pub time_secs: Option<u32>,
    /// Degrees north.
    pub lat: Option<f64>,
    /// Degrees east.
    pub lon: Option<f64>,
    /// Meters above mean sea level.
    pub alt_m: Option<f64>,
    /// Ground speed in km/h.
    pub speed_kmh: Option<f64>,
    /// Horizontal dilution of precision.
    pub hdop: Option<f64>,
    /// HDOP times a nominal ranging error, in meters: an estimate, not a measurement.
    pub accuracy_m: Option<f64>,
    /// Every satellite in view: used first, then strongest first, then by system and PRN.
    pub satellites: Vec<SatelliteShown>,
}

/// A UTC date, as a receiver's `RMC` gives it.
#[uniffi::remote(Record)]
pub struct SkyDate {
    pub year: i32,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
}

/// One satellite in view.
#[uniffi::remote(Record)]
pub struct SatelliteShown {
    /// Its constellation.
    pub system: Constellation,
    /// Its PRN within that constellation.
    pub prn: u16,
    /// Signal to noise in dB-Hz. `None` when in view and not tracked.
    pub snr: Option<u8>,
    /// The solution uses it.
    pub used: bool,
}
