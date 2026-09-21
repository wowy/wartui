//! What this firmware asks of the radio.
//!
//! Parking is not `set_channel` alone — see [`park`]. A node that sniffs has
//! promiscuous mode on for its own reasons, so that falls out for free on the dwell
//! side; the hop back to the control channel is the one to check on hardware.
//!
//! [`set_broadcast_rate`] and [`set_tx_power`] are the two direct IDF calls this
//! firmware needs because `esp-radio` does not expose them once its long-lived handles
//! borrow the controller.

use esp_radio::esp_now::{EspNowManager, EspNowSender};
use esp_radio::wifi::sniffer::Sniffer;
use wartui_proto::link::BROADCAST;

/// Tune the radio, saying whether it took.
///
/// `listen` decides whether promiscuous mode is left on afterwards: on for a
/// dwell, off on the control channel, where every data frame in the room would
/// otherwise reach the callback for nothing.
///
/// Promiscuous mode is switched on across the change either way, and it is not
/// decoration: on an
/// unassociated station interface the channel does not stick without it, and a node
/// whose channel silently did not change reports the right networks against the wrong
/// frequency and hears no assignment.
///
/// A refusal is worth reporting rather than retrying: it is the blob declining a
/// channel outright, and both known causes — an unset band mode, or a channel outside
/// the regulatory domain — are settled before the first sweep.
pub fn park(manager: &EspNowManager<'_>, sniffer: &Sniffer<'_>, channel: u8, listen: bool) -> bool {
    let _ = sniffer.set_promiscuous_mode(true);
    let parked = manager.set_channel(channel).is_ok();
    if !listen {
        let _ = sniffer.set_promiscuous_mode(false);
    }
    parked
}

/// Broadcast at 802.11g 24 Mbps rather than ESP-NOW's 1 Mbps default, saying whether
/// that took.
///
/// Every frame a node sends is a broadcast, so the rate goes on the broadcast peer
/// `esp-radio` registers at init, once; a node never removes that peer.
/// `set_peer_rate` in `firmware/bridge/src/main.rs` has why 24 Mbps, what it costs,
/// and why this cannot go through `esp-radio`.
#[allow(unsafe_code, reason = "esp-radio's own espnow-rate call is refused on the C5 and C6")]
pub fn set_broadcast_rate(_manager: &EspNowManager<'_>) -> bool {
    #[cfg(feature = "esp32c5")]
    use esp_wifi_sys_esp32c5::include as sys;
    #[cfg(feature = "esp32c6")]
    use esp_wifi_sys_esp32c6::include as sys;

    let mut config = sys::esp_now_rate_config_t {
        phymode: sys::wifi_phy_mode_t_WIFI_PHY_MODE_11G,
        rate: sys::wifi_phy_rate_t_WIFI_PHY_RATE_24M,
        ersu: false,
        dcm: false,
    };
    // SAFETY: `BROADCAST` is six readable bytes and `config` is a fully initialised
    // `esp_now_rate_config_t`, both alive for the whole call. ESP-NOW is initialised,
    // since an `EspNowManager` exists. 0 is `ESP_OK`.
    unsafe { sys::esp_now_set_peer_rate_config(BROADCAST.as_ptr(), &mut config) == 0 }
}

/// Set the Wi-Fi transmit power after ESP-NOW has borrowed the controller.
///
/// `WifiController::set_max_tx_power` is safe but requires `&mut self`; the node keeps
/// the sniffer and ESP-NOW handles borrowed for its whole life, so this is the narrow
/// direct IDF equivalent. The caller applies it only when adopting a fresh assignment.
///
/// The manager goes unused for the reason [`set_broadcast_rate`] takes one: IDF wants
/// `esp_wifi_start` behind this call, and a witness is what stops it being made from
/// somewhere in `main` that compiles and then fails on the board.
#[allow(
    unsafe_code,
    reason = "esp-radio requires a mutable controller after long-lived handles borrow it"
)]
pub fn set_tx_power(_manager: &EspNowManager<'_>, power: i8) -> bool {
    #[cfg(feature = "esp32c5")]
    use esp_wifi_sys_esp32c5::include as sys;
    #[cfg(feature = "esp32c6")]
    use esp_wifi_sys_esp32c6::include as sys;

    // SAFETY: Wi-Fi has started before the node enters its receive loop, and this
    // passes the scalar ESP-IDF expects. The function changes only the radio's
    // maximum transmit power.
    unsafe { sys::esp_wifi_set_max_tx_power(power) == 0 }
}

/// Put a frame on the air for whoever is listening.
///
/// Broadcast, because that is how a plaintext node reaches its core and because it
/// needs no peer slot on either end. Nothing acknowledges a broadcast, so the return
/// value says only that the radio transmitted it — the ack that matters runs the
/// other way, when the host unicasts an assignment.
pub fn broadcast(sender: &mut EspNowSender<'_>, frame: &[u8]) -> bool {
    // `esp-radio` registers the broadcast peer at init, and the waiter's `Drop`
    // blocks anyway, so waiting costs nothing that walking away would save.
    sender.send(&BROADCAST, frame).is_ok_and(|waiter| waiter.wait().is_ok())
}
