use std::time::Instant;

use wartui_bridge::LinkEvent;
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
// `Link` carries an inline ESP-NOW payload. Boxing it would allocate per frame to shrink a value
// that lives inside one `handle` call.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    /// Traffic, or the lack of it, from the bridge.
    Link(LinkEvent),
    /// The periodic tick. Drives liveness ageing and the status poll.
    Tick,
    /// Something the operator asked for.
    Command(Command),
    /// The runtime's periodic reading of the store and host health. The engine adds its counters
    /// and lag peak and records a [`Record::HostStatus`].
    HostSample(HostSample),
}

/// An operator's instruction. An event rather than a method, so [`FleetEngine::handle`] orders
/// keypresses against heartbeats, testably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Give one node the Bluetooth scan as its whole job, or take the scan off the fleet.
    ///
    /// At most one node, and by default none. The two radios share one 2.4 GHz antenna, and a scan
    /// holds it through the window a node listens in. That cost a stock node every assignment sent
    /// to it (`docs/phase-0-findings.md`). A node sniffing no Wi-Fi has nothing to hold the antenna
    /// against, so Bluetooth costs the fleet a whole sniffer and buys back-to-back scans.
    ///
    /// Nothing is sent now. The planner's next re-cut deals that node's channels to the rest.
    /// Moving the scan costs two frames, since the old holder gets a share back. While remembering
    /// is on, the target also becomes the preferred node, `None` included, so a withdrawn scan
    /// stays withdrawn.
    AssignBle {
        /// Which node, or `None` to stop scanning BLE anywhere.
        mac: Option<Mac>,
    },

    /// Turn remembering the preferred Bluetooth node on or off.
    ///
    /// Off forgets the preferred node but leaves the scan where it is. On from off adopts the
    /// current holder. Setting the current value is a no-op.
    RememberBle {
        /// Whether to remember.
        on: bool,
    },

    /// Change the fleet's transmit powers, in ESP-IDF quarter-dBm units.
    ///
    /// Both are clamped as [`FleetEngine::new`] clamps them. The bridge's power goes out at once
    /// (`FleetEngine::update_bridge_tx_power`), the nodes' in re-sent assignments
    /// (`FleetEngine::update_nodes_tx_power`). Setting the current values is a no-op.
    SetTxPower {
        /// Wi-Fi transmit power for the nodes.
        nodes: i8,
        /// Wi-Fi transmit power for the bridge.
        bridge: i8,
    },

    /// Ask a node, or every assignable node, to empty its dedup ring. Each clear goes out in that
    /// node's next admin window, like an assignment.
    ClearRing {
        /// Which node, or `None` for every node [`FleetEngine::is_assignable`] when this arrives.
        mac: Option<Mac>,
    },

    /// Change the channel pool the fleet partitions.
    ///
    /// The planner re-cuts the fleet, and each node takes its new share in its next admin window
    /// under a fresh epoch. A node whose share changes empties its own dedup ring. Setting the
    /// current pool is a no-op.
    SetPool {
        /// The pool to partition from now on.
        pool: ChannelPool,
    },
}

/// What the engine wants done as a result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ActionBatch {
    /// Rows for the store, in the order they should be written.
    pub records: Vec<Record>,
    /// Commands that can wait behind anything else.
    pub bulk: Vec<wartui_proto::link::HostToBridge>,
    /// Commands sent ahead of `bulk`. Assignments and dedup-ring clears race a node's 100 ms admin
    /// window straight after a heartbeat. Peer removals come here because `bulk` can drop them
    /// (`FleetEngine::evict_stale_peers`).
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
