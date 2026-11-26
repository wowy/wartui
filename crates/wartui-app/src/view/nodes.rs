//! Each node's row: its state, how much that asks of the operator, its share and whether it can
//! be power-cycled.

use wartui_core::engine::{NodeView, Snapshot};
use wartui_proto::bus::{Board, Capabilities, Compat, NODE_VERSION, Version};
use wartui_proto::fleet::ScanMode;
use wartui_proto::mac;
use wartui_proto::plan::SCAN_CHANNELS;

/// One node, as of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// `status`'s severity, raised to a warning for an otherwise healthy node that is hot or has a
    /// supply drop.
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

impl NodeRow {
    /// What the operator calls the node: its slot's label, or the tail of its address with no slot.
    #[must_use]
    pub fn name(&self) -> &str {
        self.slot.as_ref().map_or(&self.mac_tail, |slot| &slot.label)
    }

    /// `node`'s row in `snapshot`.
    pub(super) fn of(snapshot: &Snapshot, node: &NodeView) -> Self {
        let state = &node.state;
        let (status, severity) = status(node);
        let warns = state.hot() || state.supply_dropped();
        Self {
            mac: mac::full(&state.mac).to_string(),
            mac_tail: mac::short(&state.mac).to_string(),
            slot: state.slot.map(|slot| SlotShown {
                index: slot.as_u8(),
                label: snapshot.node_board().label(slot).to_string(),
            }),
            board: state.capabilities.map(|caps| caps.board),
            firmware: state.capabilities.map(|caps| firmware(caps, NODE_VERSION)),
            status,
            severity: if severity == Severity::Ok && warns { Severity::Warning } else { severity },
            share: share(node, snapshot.scan == Some(ScanMode::Stopped)),
            sightings: state.observations,
            batches_lost: state.batches_lost,
            die_temp_c: state.die_temp.c(),
            hot: state.hot(),
            supply_dropped: state.supply_dropped(),
            power_cycle: power_cycle(snapshot, node),
        }
    }
}

/// A node's slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotShown {
    /// The slot's byte, which `Slot::from_u8` turns back into the slot.
    pub index: u8,
    /// The slot's label on the board the bridge announced: `N7` on a carrier, `B1` with none.
    pub label: String,
}

/// A node's firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareShown {
    /// The version it runs, `0.7.0`.
    pub version: String,
    /// This build's node version, when the node's is older.
    pub update_to: Option<String>,
}

/// The firmware `caps` announce, against `here`, the node version this host was built with.
fn firmware(caps: Capabilities, here: Version) -> FirmwareShown {
    FirmwareShown {
        version: caps.version.to_string(),
        update_to: here.compat(&caps.version).node_behind().then(|| here.to_string()),
    }
}

/// What the operator most needs to know about a node, first match first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Heartbeats have stopped, or it is not yet drivable: the bridge deals it no share.
    Stale,
    /// It restarted under the capture.
    Rebooted {
        /// How many times.
        times: u32,
    },
    /// Nothing to do.
    Alive,
}

/// How much a node's state asks of the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// A node's status and its severity.
///
/// Another major or minor comes ahead of `NoHeartbeat`, because such a node's heartbeats are read
/// only as far as its version and never count as liveness. `NoSlot` comes ahead of `Stale`: a
/// node whose slot another took is unreachable until its own heartbeat names a slot again.
fn status(node: &NodeView) -> (NodeStatus, Severity) {
    let state = &node.state;
    match state.capabilities.map(|caps| caps.version.compat_with_host()) {
        Some(Compat::NodeBehind) => return (NodeStatus::NeedsUpdate, Severity::Fault),
        Some(Compat::NodeAhead) => return (NodeStatus::HostBehind, Severity::Fault),
        _ => {}
    }
    if node.starting {
        return (NodeStatus::Starting, Severity::Ok);
    }
    if state.last_heartbeat.is_none() {
        return (NodeStatus::NoHeartbeat, Severity::Warning);
    }
    if state.slot.is_some() && !node.on_bus {
        return (NodeStatus::OffBus, Severity::Warning);
    }
    if state.slot.is_none() {
        return (NodeStatus::NoSlot, Severity::Warning);
    }
    if !node.assignable {
        return (NodeStatus::Stale, Severity::Warning);
    }
    if state.reboots > 0 {
        return (NodeStatus::Rebooted { times: state.reboots }, Severity::Notice);
    }
    (NodeStatus::Alive, Severity::Ok)
}

/// What a node scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareShown {
    /// Wi-Fi channels, as channel numbers in scan-table order. `pending`: in the bridge's plan, not
    /// yet adopted by the node.
    Channels {
        /// The channel numbers.
        channels: Vec<u8>,
        /// In the plan, not yet adopted.
        pending: bool,
    },
    /// The Bluetooth scan, which holds no channels.
    Bluetooth {
        /// In the plan, not yet adopted.
        pending: bool,
    },
    /// No share while the bridge's scan is stopped.
    Idle,
    /// No share while the bridge scans.
    Unassigned,
}

/// The node's share of the bridge's plan, pending until its heartbeat carries the share's epoch.
fn share(node: &NodeView, stopped: bool) -> ShareShown {
    let Some(held) = node.share else {
        return if stopped { ShareShown::Idle } else { ShareShown::Unassigned };
    };
    let pending = !node.adopted;
    if held.ble {
        return ShareShown::Bluetooth { pending };
    }
    let channels = held
        .channels
        .indices()
        .filter_map(|idx| SCAN_CHANNELS.get(usize::from(idx)).copied())
        .collect();
    ShareShown::Channels { channels, pending }
}

/// Whether a power-cycle of a node can be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerCycle {
    /// It can.
    Allowed,
    /// The link is down. The transport holds commands while it reconnects, so one sent now would
    /// cut the node whenever the bridge comes back.
    NotConnected,
    /// The bridge has no carrier board to switch node power with.
    NoCarrier,
    /// The node has not heartbeated, so nothing says which slot it is in.
    NotHeard,
    /// Another node's heartbeat took its slot.
    LostSlot,
}

/// The refusals in order: link down, then no carrier, then no slot. A node needs only a slot: one
/// that has stopped heartbeating is what a power-cycle is for.
fn power_cycle(snapshot: &Snapshot, node: &NodeView) -> PowerCycle {
    if !snapshot.link_up {
        return PowerCycle::NotConnected;
    }
    if !snapshot.node_board().is_carrier() {
        return PowerCycle::NoCarrier;
    }
    match (node.state.slot, node.state.capabilities) {
        (Some(_), _) => PowerCycle::Allowed,
        (None, None) => PowerCycle::NotHeard,
        (None, Some(_)) => PowerCycle::LostSlot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(version: Version) -> Capabilities {
        Capabilities { version, ..Capabilities::here(Board::Esp32c5) }
    }

    #[test]
    fn firmware_names_update_when_node_patch_is_behind() {
        let shown = firmware(caps(Version::new(0, 1, 2)), Version::new(0, 1, 3));
        assert_eq!(shown.version, "0.1.2");
        assert_eq!(shown.update_to.as_deref(), Some("0.1.3"));
    }

    #[test]
    fn firmware_names_no_update_when_node_is_current() {
        let shown = firmware(caps(Version::new(0, 1, 2)), Version::new(0, 1, 2));
        assert_eq!(shown, FirmwareShown { version: "0.1.2".to_owned(), update_to: None });
    }

    #[test]
    fn firmware_names_no_update_when_node_is_on_trial() {
        let version = Version::new(0, 1, 2);
        let trial = Capabilities { trial: true, five_ghz: true, ..caps(version) };
        assert_eq!(firmware(trial, version).update_to, None);
    }

    #[test]
    fn firmware_names_no_update_when_node_runs_newer_minor() {
        let shown = firmware(caps(Version::new(0, 2, 0)), Version::new(0, 1, 3));
        assert_eq!(shown.update_to, None);
    }
}
