use std::time::Duration;

use indexmap::IndexSet;
use wartui_proto::bus::{ACK_TIMEOUT_MS, SEND_TRIES};
use wartui_proto::mac::Mac;

#[cfg(doc)]
use super::{Command, FleetEngine};
use crate::position::PositionChain;
use crate::ubx;

/// How long a record sent to a node waits for its `Delivered` before it is treated as
/// undelivered: a second past the bridge's own give-up, so the bridge's answer comes first. A
/// lost answer never comes.
pub const SEND_TIMEOUT: Duration =
    Duration::from_millis(ACK_TIMEOUT_MS * SEND_TRIES as u64 + 1_000);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// How long without a heartbeat before a node stops counting towards topology, fixed: no config
    /// key or flag sets it. 20 s rides out three missed 5 s heartbeats in a row.
    pub topology_timeout: Duration,
    /// How often to ask the bridge for its counters.
    pub status_interval: Duration,
    /// How many recent observations the snapshot carries for the UI.
    pub tail_len: usize,
    /// Keep the undecoded bytes of every frame as well.
    pub record_raw: bool,
    /// Where the host believes it is.
    pub position: PositionChain,
    /// How long to wait for the bridge's verdict on an assignment or a clear before recording it
    /// unanswered: the host's backstop for a bridge that never answers. The bridge answers
    /// `Undelivered` itself once its tries run out, so the default, [`SEND_TIMEOUT`], waits past
    /// that.
    pub admin_timeout: Duration,
    /// Whether [`Command::AssignBle`] records its target as the preferred Bluetooth node.
    pub remember_ble: bool,
    /// The node given the Bluetooth scan whenever it is assignable and no node holds
    /// the scan. Ignored when [`Self::remember_ble`] is off.
    pub preferred_ble: Option<Mac>,
    /// SSIDs never captured. A Wi-Fi sighting whose SSID equals an entry byte-for-byte is dropped
    /// before anything counts or records it. A Bluetooth name is not an SSID, so BLE is never
    /// dropped. [`Command::SetExcludedSsids`] replaces the list mid-run. A set for the lookup
    /// every Wi-Fi sighting makes, and one that keeps the operator's order, which the settings
    /// modal shows.
    pub excluded_ssids: IndexSet<Vec<u8>>,
    /// How often the bridge's GPS receiver reports a fix, in hertz. `FleetEngine::poll_bridge` says
    /// why every status poll carries it.
    pub gps_rate_hz: u8,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            topology_timeout: Duration::from_secs(20),
            status_interval: Duration::from_secs(5),
            tail_len: 200,
            record_raw: false,
            position: PositionChain::empty(),
            admin_timeout: SEND_TIMEOUT,
            remember_ble: true,
            preferred_ble: None,
            excluded_ssids: IndexSet::new(),
            gps_rate_hz: ubx::DEFAULT_RATE_HZ,
        }
    }
}
