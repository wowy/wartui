use std::time::Instant;

use indexmap::IndexSet;
use wartui_bridge::LinkEvent;
use wartui_proto::bus::Slot;
use wartui_proto::fan::FanDuty;
use wartui_proto::plan::ChannelPool;

#[cfg(doc)]
use super::{EngineConfig, FleetEngine};
use crate::health::Health;
use crate::record::Record;

/// The time, in both forms this code needs.
///
/// Durations use the monotonic clock, so an NTP correction cannot declare the fleet dead. Stored
/// timestamps use the wall clock, so a capture lines up against other records. Both travel together
/// so a caller cannot reach for whichever is nearest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// Monotonic, for elapsed-time decisions.
    pub mono: Instant,
    /// Unix milliseconds, for anything written down.
    pub unix_ms: i64,
}

/// Something the engine must react to.
// `Link` carries an inline node record. Boxing it would allocate per frame to shrink a value
// that lives inside one `handle` call.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    /// Traffic, or the lack of it, from the bridge.
    Link(LinkEvent),
    /// The periodic tick. Drives liveness aging and the status poll.
    Tick,
    /// Something the operator asked for.
    Command(Command),
    /// The runtime's periodic reading of the store and host health. The engine adds its counters
    /// and lag peak and records a [`Record::HostStatus`].
    HostSample(HostSample),
}

/// An operator's instruction. An event rather than a method, so [`FleetEngine::handle`] orders
/// keypresses against heartbeats, testably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Ask the bridge to scan Wi-Fi, and Bluetooth too when `true`.
    ///
    /// The P4 gives Bluetooth to one node as its whole job, once two nodes can take a share. The
    /// two radios share one 2.4 GHz antenna, and a scan holds it through the window a node listens
    /// in. That cost a stock node every assignment sent to it (`docs/phase-0-findings.md`). A
    /// node sniffing no Wi-Fi has nothing to hold the antenna against, so Bluetooth costs the
    /// fleet a whole sniffer and buys back-to-back scans.
    ///
    /// Sent at once, with a `GetStatus` behind it, while the run wants a scan, and it becomes the
    /// mode the run restarts a reset bridge in. While a run is still starting the scan, it sets
    /// the mode the start uses. A fleet the operator stopped ignores it.
    /// [`EngineConfig::auto_start`] is how a run starts: a P4 found stopped starts on Wi-Fi, and
    /// one found scanning is stopped and started again in the mode it held, so a capture starts
    /// with every node's dedup ring empty. Later in the run, a P4 found scanning is left alone and
    /// one found stopped is started again.
    SetBluetooth(bool),

    /// Ask the bridge to idle every node, as a clean quit and an update do. Sent at once, with a
    /// `GetStatus` behind it. The run does not start the scan again.
    StopScan,

    /// Ask the bridge to hold the channel pool `pool`, and keep it across its reboots.
    ///
    /// Sent once, at once, with a `GetStatus` behind it, as [`Command::SetFanDuty`] is. The P4's
    /// planner cuts it at once, and its next `Fleet` report shows the new shares.
    SetPool {
        /// The pool for the bridge to hold.
        pool: ChannelPool,
    },

    /// Replace the SSIDs never captured ([`EngineConfig::excluded_ssids`]). Takes effect from the
    /// next sighting; nothing goes to the nodes.
    SetExcludedSsids {
        /// Each SSID's bytes, matched exactly, in the operator's order.
        ssids: IndexSet<Vec<u8>>,
    },

    /// Ask the bridge's carrier board to switch the node in `slot` off and on.
    ///
    /// Sent at once, and answered by the bridge whatever it carries; the answer reaches the
    /// operator as [`Snapshot::power_answer`](super::Snapshot::power_answer). The node reboots,
    /// its heartbeats say so, and the P4 sends it its share again.
    PowerCycle {
        /// Which node, by the slot its heartbeat came from.
        slot: Slot,
    },

    /// Ask the bridge to run the carrier's lid fan at `duty`, and keep it across its reboots.
    ///
    /// Sent once, at once, with a `GetStatus` behind it, so
    /// [`Snapshot::bridge_status`](super::Snapshot::bridge_status) shows the duty in force without
    /// waiting for the next poll. The bridge stores it, not the host. A bridge with no carrier
    /// ignores it.
    SetFanDuty {
        /// The duty to run.
        duty: FanDuty,
    },
}

/// What the engine wants done as a result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActionBatch {
    /// Rows for the store, in the order they should be written.
    pub records: Vec<Record>,
    /// Commands that can wait behind anything else.
    pub bulk: Vec<wartui_proto::link::HostToBridge>,
    /// Commands sent ahead of `bulk`: the scan's start and stop.
    pub urgent: Vec<wartui_proto::link::HostToBridge>,
}

impl ActionBatch {
    /// Whether there is anything to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.bulk.is_empty() && self.urgent.is_empty()
    }
}

/// Counters the store keeps, folded into the snapshot for display.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    /// Rows written.
    pub written: u64,
    /// Rows dropped because the store fell behind. Losing an observation beats stalling the engine,
    /// but it must show.
    pub dropped: u64,
}

/// The store's largest figures since they were last taken, for a [`HostSample`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StorePeaks {
    /// The deepest the queue got, in records.
    pub queue: u64,
    /// The slowest batch, statements and commit together, in microseconds.
    pub commit_us: u64,
}

/// What the runtime read for one [`Event::HostSample`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HostSample {
    /// The store's running totals.
    pub store: StoreStats,
    /// The store's peaks since the previous sample.
    pub peaks: StorePeaks,
    /// The host's own health.
    pub health: Health,
}
