//! The Wi-Fi transmit power the host configures, and the range it permits.

/// The default Wi-Fi transmit power, in ESP-IDF's quarter-dBm units.
///
/// 8 is 2 dBm, the lowest value `set_max_tx_power` accepts. The host supplies this
/// default to every bridge it connects and in every node assignment, so firmware does
/// not hard-code the fleet policy.
pub const DEFAULT_TX_POWER_QUARTER_DBM: i8 = 8;

/// The lowest transmit power `set_max_tx_power` accepts: 2 dBm.
pub const MIN_TX_POWER_QUARTER_DBM: i8 = 8;

/// The highest transmit power the host will configure: 20 dBm.
///
/// `set_max_tx_power` accepts up to 84 (21 dBm), but whether values above 20 dBm
/// behave correctly is unverified, so nothing higher than this leaves the host.
/// The ceiling is policy, then, not the IDF's — and the firmware carries no
/// maximum of its own, because this is the only place one is needed.
pub const MAX_TX_POWER_QUARTER_DBM: i8 = 80;

/// Bring a transmit power inside the range the host permits.
///
/// Every power that reaches a radio goes through this, because a refused one is close
/// to invisible on a node: the refusal is a `note!`, which is a discarded
/// `format_args!` without the `log` feature, so the node keeps its boot power while
/// the host's snapshot, the panel's RSSI threshold and the link-budget reasoning in
/// `set_peer_rate` all read as though the configured value applied. Clamping at the
/// host is what keeps the fleet and the model that describes it from diverging
/// silently.
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
