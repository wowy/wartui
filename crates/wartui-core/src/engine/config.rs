use std::time::Duration;

use wartui_proto::mac::Mac;
use wartui_proto::plan::{ChannelPool, DEFAULT_TX_POWER_QUARTER_DBM};

#[cfg(doc)]
use super::{Command, FleetEngine};
use crate::position::PositionChain;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Which channels the fleet is meant to scan: the set the engine partitions
    /// across the fleet, and shown in the UI. The capture records the value it
    /// starts with; [`Command::SetPool`] changes it mid-run.
    pub pool: ChannelPool,
    /// How long a node may go without a heartbeat before it stops counting
    /// towards topology. Matches the firmware's own 60 s node timeout.
    pub topology_timeout: Duration,
    /// How often to ask the bridge for its counters.
    pub status_interval: Duration,
    /// How many recent observations the snapshot carries for the UI.
    pub tail_len: usize,
    /// Keep the undecoded bytes of every frame as well.
    pub record_raw: bool,
    /// Where the host believes it is.
    pub position: PositionChain,
    /// How long to wait for a bridge to report what became of an assignment
    /// before writing it down as unanswered. Generous: the bridge blocks on
    /// the transmit callback, and a busy radio can take tens of milliseconds.
    pub admin_timeout: Duration,
    /// Wi-Fi transmit power sent to every node in its assignment, in ESP-IDF
    /// quarter-dBm units.
    ///
    /// [`FleetEngine::new`] brings it inside the range the host permits, so a value
    /// outside it costs the fleet a clamp rather than leaving every radio at its
    /// boot power. [`Command::SetTxPower`] clamps it the same way when it changes
    /// mid-run.
    pub tx_power: i8,
    /// Wi-Fi transmit power sent to the bridge with every status poll, in ESP-IDF
    /// quarter-dBm units.
    ///
    /// A separate setting from [`Self::tx_power`] because the bridge's job is not a
    /// node's: its transmissions are assignments, not the heartbeats and sightings a
    /// fleet is positioned for. Clamped at construction alongside `tx_power`, and
    /// again by [`Command::SetTxPower`] when it changes mid-run.
    pub bridge_tx_power: i8,
    /// Whether [`Command::AssignBle`] records its target as the preferred Bluetooth node.
    pub remember_ble: bool,
    /// The node given the Bluetooth scan whenever it is assignable and no node holds
    /// the scan. Ignored when [`Self::remember_ble`] is off.
    pub preferred_ble: Option<Mac>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            pool: ChannelPool::All,
            topology_timeout: Duration::from_secs(60),
            status_interval: Duration::from_secs(5),
            tail_len: 200,
            record_raw: false,
            position: PositionChain::empty(),
            admin_timeout: Duration::from_secs(2),
            tx_power: DEFAULT_TX_POWER_QUARTER_DBM,
            bridge_tx_power: DEFAULT_TX_POWER_QUARTER_DBM,
            remember_ble: true,
            preferred_ble: None,
        }
    }
}
