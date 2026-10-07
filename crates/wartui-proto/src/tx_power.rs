//! The Wi-Fi transmit power the host configures, and the range it permits.

/// The default Wi-Fi transmit power, in ESP-IDF's quarter-dBm units.
///
/// 8 is 2 dBm, the lowest value `set_max_tx_power` accepts. It is the default for the
/// host's fleet and bridge settings. The host sends the configured bridge power with every
/// status poll. It sends the configured fleet power in every assignment. So no firmware
/// hard-codes the fleet policy.
pub const DEFAULT_TX_POWER_QUARTER_DBM: i8 = 8;

/// The lowest transmit power `set_max_tx_power` accepts: 2 dBm.
pub const MIN_TX_POWER_QUARTER_DBM: i8 = 8;

/// The highest transmit power the host will configure: 20 dBm.
///
/// `set_max_tx_power` accepts up to 84 (21 dBm), but whether values above 20 dBm behave
/// correctly is unverified. Nothing higher than this leaves the host. The ceiling is
/// policy, not the IDF's, and the firmware carries no maximum of its own.
pub const MAX_TX_POWER_QUARTER_DBM: i8 = 80;

/// Bring a transmit power inside the range the host permits.
///
/// Every power that reaches a radio goes through this, because a node's refusal is
/// nearly invisible. The refusal is a `note!`, which compiles to nothing without the
/// `log` feature. The node keeps its boot power, while the host's snapshot, the panel's
/// RSSI threshold and the link budget in `set_peer_rate` all assume the configured
/// value. Clamping at the host keeps the fleet and its model from diverging silently.
#[must_use]
pub const fn clamp_tx_power(power: i8) -> i8 {
    // Written out because `Ord::clamp` is not a `const fn`.
    if power < MIN_TX_POWER_QUARTER_DBM {
        MIN_TX_POWER_QUARTER_DBM
    } else if power > MAX_TX_POWER_QUARTER_DBM {
        MAX_TX_POWER_QUARTER_DBM
    } else {
        power
    }
}

const _: () = assert!(
    DEFAULT_TX_POWER_QUARTER_DBM == clamp_tx_power(DEFAULT_TX_POWER_QUARTER_DBM),
    "the host configures 8 to 80 quarter-dBm: the IDF's floor, and 20 dBm at the top"
);
