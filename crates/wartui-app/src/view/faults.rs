//! What the operator should know: notes, which are no fault, and faults, most urgent first.
//!
//! A Bluetooth link the bridge let go because a USB host took it is down, but nothing is broken:
//! the bridge serves one host, USB first, and hands the link back once USB goes quiet. So it is a
//! note. So is the transport waiting on the OS's pairing dialog, or past it on the bridge's
//! answer: the link is connected, and the operator has a dialog to answer or nothing to do.
//!
//! Counts a healthy capture also produces, such as refused sightings and duplicate batches, are
//! not faults either, so a clean run has none.

use wartui_bridge::DisconnectCause;
use wartui_core::engine::Snapshot;
use wartui_core::gps::{GpsStatus, GpsView};
use wartui_proto::fan::FanDuty;
use wartui_proto::plan::{ChannelPool, SCAN_CHANNELS};
use wartui_proto::reset::ResetCause;

use super::NodeRow;
#[cfg(doc)]
use super::View;

/// Something an operator should know that is no fault. [`View::notes`] lists them in the order a
/// front end shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// Sightings a full pending buffer turned away. At least one count is non-zero. Most are
    /// reported on a later dwell or scan, so this measures how dense the area is.
    BufferFull {
        /// Wi-Fi sightings turned away.
        wifi: u64,
        /// Bluetooth sightings turned away.
        bluetooth: u64,
    },
    /// The link is down because a USB host took the bridge from this Bluetooth host.
    UsbHoldsBridge,
    /// The link waits on the OS's pairing. `notice` is the transport's.
    WaitingOnPairing {
        /// The transport's notice.
        notice: String,
    },
    /// The subscription is through and the bridge has not answered. `notice` is the transport's.
    WaitingOnReady {
        /// The transport's notice.
        notice: String,
    },
}

/// Something wrong with the capture, the bridge, the receiver or a node. [`View::faults`] lists
/// every one so far, most urgent first, and is empty on a clean run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// The link is down. First, because with the link down nothing else updates.
    LinkDown(LinkFault),
    /// The store fell behind and dropped rows.
    StoreDropped {
        /// Rows dropped.
        rows: u64,
    },
    /// Channels of the pool in no share of the bridge's plan: no node can tune them.
    Unswept {
        /// The pool.
        pool: ChannelPool,
        /// The channel numbers left out, in table order.
        channels: Vec<u8>,
    },
    /// The GPS receiver's port could not be opened or stopped reading.
    GpsUnreadable {
        /// The reader's account of it.
        reason: String,
    },
    /// The receiver sends lines that fail their checksum, and no fix.
    GpsRejects {
        /// Lines rejected.
        lines: u64,
        /// Where the lines come from, which says what to suspect.
        from: RejectsFrom,
    },
    /// Node records of an unknown type, or that would not parse.
    Undecodable {
        /// How many.
        frames: u64,
    },
    /// Link frames that failed their checksum.
    Garbled {
        /// How many.
        frames: u64,
    },
    /// Node records in a layout this build does not read: a fleet half-way through a reflash.
    Incompatible {
        /// How many.
        frames: u64,
    },
    /// Frames the bridge dropped since this host attached: data this capture lost.
    BridgeDropped {
        /// How many.
        frames: u64,
    },
    /// The bridge restarted under the capture. Never `ResetCause::PowerOn`, which is how a capture
    /// starts.
    BridgeRestarted {
        /// Why.
        cause: ResetCause,
    },
    /// No tach pulses at a duty the fan turns at: unplugged or stalled.
    FanStopped {
        /// The duty in force, in percent.
        duty_pct: u8,
    },
    /// A node restarted from a fault.
    NodeRestarted {
        /// The node's name ([`NodeRow::name`]).
        node: String,
        /// Its latest fault restart's cause.
        cause: ResetCause,
        /// How many fault restarts the host saw.
        times: u32,
    },
}

/// Why the link is down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkFault {
    /// What kind of failure the transport names.
    pub kind: LinkFaultKind,
    /// The transport's own account of it.
    pub detail: String,
}

/// What the transport tells apart of a link failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFaultKind {
    /// The bridge refused this host: it holds no bond for it.
    NotBonded,
    /// The bridge dropped this host before serving it, as it drops a host it holds no bond for.
    MaybeNotBonded,
    /// Nothing the transport names.
    Other,
}

/// Where a receiver's rejected lines come from. Only a rate the operator set is worth blaming: a
/// rate the search settled on already decoded, so later rejects mean a changed receiver or a bad
/// cable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectsFrom {
    /// The bridge's receiver, which has no port or rate to name.
    Bridge,
    /// A host receiver at the rate the operator set.
    PinnedBaud,
    /// A host receiver at the port and rate the search settled on.
    Port {
        /// The port.
        port: String,
        /// The rate.
        baud: u32,
    },
    /// A host receiver the search has not settled on.
    Unknown,
}

/// Rejected lines with no fix past this suggest the wrong rate rather than joining a stream
/// mid-sentence.
const REJECTS_FAULT_PAST: u64 = 20;

/// The snapshot's notes and faults, naming nodes as `nodes` do.
pub(super) fn of(snapshot: &Snapshot, nodes: &[NodeRow]) -> (Vec<Note>, Vec<Fault>) {
    let c = &snapshot.counters;
    let mut notes = Vec::new();
    let mut faults = Vec::new();
    if c.wifi_refused > 0 || c.ble_refused > 0 {
        notes.push(Note::BufferFull { wifi: c.wifi_refused, bluetooth: c.ble_refused });
    }
    link(snapshot, &mut notes, &mut faults);
    if snapshot.store.dropped > 0 {
        faults.push(Fault::StoreDropped { rows: snapshot.store.dropped });
    }
    if let Some(pool) = snapshot.pool
        && !snapshot.unreachable.is_empty()
    {
        let channels = snapshot
            .unreachable
            .indices()
            .filter_map(|idx| SCAN_CHANNELS.get(usize::from(idx)).copied())
            .collect();
        faults.push(Fault::Unswept { pool, channels });
    }
    if let Some(gps) = &snapshot.gps {
        gps_faults(gps, &mut faults);
    }
    if c.undecodable > 0 {
        faults.push(Fault::Undecodable { frames: c.undecodable });
    }
    if c.garbled > 0 {
        faults.push(Fault::Garbled { frames: c.garbled });
    }
    // A fleet half-way through a reflash is invisible to the node rows, so this is the only place
    // it shows.
    if c.incompatible > 0 {
        faults.push(Fault::Incompatible { frames: c.incompatible });
    }
    // Only this capture's loss. The bridge's own count runs from its boot, mostly frames dropped
    // before anyone was listening.
    if let Some(status) = snapshot.bridge_status
        && status.dropped_since_attach > 0
    {
        faults.push(Fault::BridgeDropped { frames: u64::from(status.dropped_since_attach) });
    }
    if let Some(bridge) = &snapshot.bridge
        && bridge.reset_cause != ResetCause::PowerOn
    {
        faults.push(Fault::BridgeRestarted { cause: bridge.reset_cause });
    }
    // Below the turning duty the fan may stop by design. With the link down the reading is stale.
    if snapshot.link_up
        && let Some(fan) = snapshot.bridge_status.and_then(|status| status.fan)
        && fan.duty >= FanDuty::TURNS_FROM
        && fan.rpm == Some(0)
    {
        faults.push(Fault::FanStopped { duty_pct: fan.duty.get() });
    }
    // After the bridge's: a bridge fault touches every node, and these touch one.
    for (view, row) in snapshot.nodes.iter().zip(nodes) {
        if let Some(cause) = view.state.fault_restart {
            let times = view.state.fault_restarts;
            faults.push(Fault::NodeRestarted { node: row.name().to_owned(), cause, times });
        }
    }
    (notes, faults)
}

/// The link's part: a note or a fault, only with the link down. Every cause is matched, so a cause
/// the transport adds is decided here before it builds.
fn link(snapshot: &Snapshot, notes: &mut Vec<Note>, faults: &mut Vec<Fault>) {
    if snapshot.link_up {
        return;
    }
    let reason = snapshot.link_error.clone();
    let fault = |kind| reason.clone().map(|detail| Fault::LinkDown(LinkFault { kind, detail }));
    match snapshot.link_cause {
        Some(DisconnectCause::UsbTookBridge) => notes.push(Note::UsbHoldsBridge),
        Some(DisconnectCause::AwaitingPairing) => {
            notes.extend(reason.map(|notice| Note::WaitingOnPairing { notice }));
        }
        Some(DisconnectCause::AwaitingReady) => {
            notes.extend(reason.map(|notice| Note::WaitingOnReady { notice }));
        }
        Some(DisconnectCause::NotBonded) => faults.extend(fault(LinkFaultKind::NotBonded)),
        Some(DisconnectCause::MaybeNotBonded) => {
            faults.extend(fault(LinkFaultKind::MaybeNotBonded));
        }
        Some(DisconnectCause::Other) | None => faults.extend(fault(LinkFaultKind::Other)),
    }
}

/// The receiver's faults. A missing receiver is none: the header names it.
fn gps_faults(gps: &GpsView, faults: &mut Vec<Fault>) {
    if let GpsStatus::Failed(reason) = &gps.status {
        faults.push(Fault::GpsUnreadable { reason: reason.clone() });
    }
    if gps.counters.fixes == 0 && gps.counters.rejected > REJECTS_FAULT_PAST {
        let from = if gps.via_bridge {
            RejectsFrom::Bridge
        } else if gps.pinned_baud {
            RejectsFrom::PinnedBaud
        } else {
            match &gps.settled {
                Some((port, baud)) => RejectsFrom::Port { port: port.clone(), baud: *baud },
                None => RejectsFrom::Unknown,
            }
        };
        faults.push(Fault::GpsRejects { lines: gps.counters.rejected, from });
    }
}
