use std::time::Instant;

use indexmap::IndexSet;
use wartui_bridge::LinkEvent;
use wartui_proto::bus::Slot;
use wartui_proto::fan::FanDuty;
use wartui_proto::mac::Mac;
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
    /// Ask the bridge to give the node in one slot the Bluetooth scan as its whole job, or to take
    /// the scan off the fleet.
    ///
    /// At most one node, and by default none. The two radios share one 2.4 GHz antenna, and a scan
    /// holds it through the window a node listens in. That cost a stock node every assignment sent
    /// to it (`docs/phase-0-findings.md`). A node sniffing no Wi-Fi has nothing to hold the antenna
    /// against, so Bluetooth costs the fleet a whole sniffer and buys back-to-back scans.
    ///
    /// The scan belongs to a slot, and the bridge holds it beside the pool, across its reboots.
    /// Sent once, at once, with a `GetStatus` behind it, as [`Command::SetPool`] is. The engine
    /// changes nothing here: the planner gives the scan to the node heartbeating from the slot the
    /// bridge's `Status` reports, and the re-cut deals that node's channels to the rest. Moving the
    /// scan costs two assignments, since the old holder gets a share back.
    AssignBle {
        /// Which slot's node, or `None` to stop scanning BLE anywhere.
        slot: Option<Slot>,
    },

    /// Ask a node, or every assignable node, to empty its dedup ring. Each clear goes out on that
    /// node's next heartbeat, like an assignment.
    ClearRing {
        /// Which node, or `None` for every node [`FleetEngine::is_assignable`] when this arrives.
        mac: Option<Mac>,
    },

    /// Ask the bridge to hold the channel pool `pool`, and keep it across its reboots.
    ///
    /// Sent once, at once, with a `GetStatus` behind it, as [`Command::SetFanDuty`] is. The engine
    /// changes nothing here: the planner deals the pool the bridge's `Status` reports, so the
    /// fleet re-cuts once that `Status` shows the change.
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
    /// operator as [`Snapshot::power_answer`](super::Snapshot::power_answer). Nothing in the
    /// plan changes: the node reboots, and its heartbeats say so.
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
    /// Commands sent ahead of `bulk`: assignments and dedup-ring clears, which answer a
    /// heartbeat.
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
