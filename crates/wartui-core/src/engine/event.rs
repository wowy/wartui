use std::time::Instant;

use wartui_bridge::LinkEvent;
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelPool;

#[cfg(doc)]
use super::{EngineConfig, FleetEngine};
use crate::health::Health;
use crate::record::Record;

/// The time, in both of the forms this code needs.
///
/// Durations are measured on the monotonic clock, which cannot jump backwards
/// over an NTP correction and declare the fleet dead; stored timestamps use the
/// wall clock, because a capture has to line up against something else. Both
/// travel together so a caller cannot reach for whichever is nearest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// Monotonic, for elapsed-time decisions.
    pub mono: Instant,
    /// Unix milliseconds, for anything written down.
    pub unix_ms: i64,
}

/// Something the engine must react to.
// `Link` dwarfs `Tick` because it carries an inline ESP-NOW payload. Boxing it
// would mean an allocation for every frame received to shrink a value
// that is created, matched on and dropped inside one call to `handle`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    /// Traffic, or the lack of it, from the bridge.
    Link(LinkEvent),
    /// The periodic tick. Drives liveness ageing and the status poll.
    Tick,
    /// Something the operator asked for.
    Command(Command),
    /// The runtime's periodic reading of what only it can see: the store and the
    /// host's own health. The engine adds its counters and lag peak and records the
    /// lot as a [`Record::HostStatus`].
    HostSample(HostSample),
}

/// An operator's instruction to the fleet.
///
/// Deliberately an event like any other rather than a method on the engine: a
/// keypress and a heartbeat have to be ordered against each other, and routing
/// both through [`FleetEngine::handle`] is what makes that ordering testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Give one node the Bluetooth scan as its whole job, or take it away from the
    /// fleet.
    ///
    /// At most one node, and by default none. The two radios share the one 2.4 GHz
    /// antenna, and a scan holds it through exactly the window a node has to be
    /// listening in — which cost a stock node every assignment sent to it
    /// (`docs/phase-0-findings.md`). A node that sniffs no Wi-Fi has nothing to
    /// hold the antenna against, which is why Bluetooth is a whole node's job
    /// rather than a slice of one's: it costs the fleet a sniffer and buys a scan
    /// run back to back with the next.
    ///
    /// Nothing goes out now, and nothing is decided now: the planner reads this on
    /// its next re-cut, which takes that node's channels away and deals them round
    /// the rest. Moving it costs two frames, because the node giving it up has a
    /// share of the pool coming back to it.
    ///
    /// While remembering is on, the target also becomes the preferred Bluetooth node,
    /// `None` included, so a scan taken off the fleet is not handed straight back.
    AssignBle {
        /// Which node, or `None` to stop scanning BLE anywhere.
        mac: Option<Mac>,
    },

    /// Turn remembering the preferred Bluetooth node on or off.
    ///
    /// Off forgets the preferred node and leaves the scan where it is: the setting is
    /// about what happens the next time the scan has no holder, not a way to stop it.
    /// On from off adopts whichever node holds the scan now. Setting the value the
    /// engine already holds is a no-op.
    RememberBle {
        /// Whether to remember.
        on: bool,
    },

    /// Change the fleet's transmit powers, in ESP-IDF quarter-dBm units.
    ///
    /// Clamped the same way [`FleetEngine::new`] clamps [`EngineConfig::tx_power`] and
    /// [`EngineConfig::bridge_tx_power`]. The bridge's power goes out at once, over the
    /// same `bulk` channel the status poll uses, when the link is up and the value
    /// changed — nothing here waits for the next poll. The nodes' power is never sent
    /// from here: it re-sends each member's share of the plan already in force under a
    /// fresh epoch, and each node picks it up on its own next heartbeat. Setting a
    /// value the engine already holds is a no-op.
    SetTxPower {
        /// Wi-Fi transmit power for the nodes.
        nodes: i8,
        /// Wi-Fi transmit power for the bridge.
        bridge: i8,
    },

    /// Ask a node, or every assignable node, to empty its dedup ring.
    ///
    /// Each node's clear goes out in its own next admin window, the same as an
    /// assignment does.
    ClearRing {
        /// Which node, or `None` for every node [`FleetEngine::is_assignable`]
        /// counts as of the moment this command arrives.
        mac: Option<Mac>,
    },

    /// Change the channel pool the fleet partitions.
    ///
    /// Nothing goes out from here: the planner re-cuts the whole fleet against the new
    /// pool, and each node takes its new share in its own next admin window under a
    /// fresh epoch. A node adopting a share that differs from the one it holds empties
    /// its dedup ring on its own. Setting the pool already in force is a no-op.
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
    /// Commands racing a node's 100 ms admin window, sent ahead of anything
    /// in `bulk`. In practice: assignments, and only ever in the moment after
    /// a heartbeat.
    pub urgent: Vec<wartui_proto::link::HostToBridge>,
}

impl ActionBatch {
    /// Whether there is anything at all to do.
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
    /// Rows dropped because the store could not keep up. Losing an observation
    /// beats stalling the engine, but it must be visible when it happens.
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
