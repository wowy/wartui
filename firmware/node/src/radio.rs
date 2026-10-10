//! What this firmware asks of the radio.
//!
//! Parking is not `set_channel` alone — see [`park`]. A node that sniffs has
//! promiscuous mode on for its own reasons, so that falls out for free on the dwell
//! side; the hop back to the control channel is the one to check on hardware.
//!
//! [`set_peer_rate`], [`set_tx_power`] and [`tx_power`] are the direct IDF calls this
//! firmware needs because `esp-radio` does not expose them once its long-lived handles
//! borrow the controller.
//!
//! A heartbeat broadcasts: it is how a bridge discovers a node in the first place,
//! before either side knows the other's address. A sighting batch unicasts, to
//! whichever core last sent this node an admin or clear frame, so the radio retries
//! it and an unacknowledged batch is knowable as such — see [`unicast`] and
//! `main.rs`'s `Outgoing`.

use esp_radio::esp_now::{EspNowError, EspNowManager, EspNowSender, EspNowWifiInterface, PeerInfo};
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

/// Send at 802.11g 24 Mbps rather than ESP-NOW's 1 Mbps default, saying whether that
/// took.
///
/// Applied to the broadcast peer `esp-radio` registers at init, once at boot, and to
/// the core peer whenever [`set_core_peer`] registers one. `set_peer_rate` in
/// `firmware/bridge/src/main.rs` has why 24 Mbps, what it costs, and why this cannot
/// go through `esp-radio`.
#[allow(unsafe_code, reason = "esp-radio's own espnow-rate call is refused on the C5 and C6")]
pub fn set_peer_rate(_manager: &EspNowManager<'_>, mac: &[u8; 6]) -> bool {
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
    // SAFETY: `mac` is six readable bytes and `config` is a fully initialized
    // `esp_now_rate_config_t`, both alive for the whole call. ESP-NOW is initialized,
    // since an `EspNowManager` exists. 0 is `ESP_OK`.
    unsafe { sys::esp_now_set_peer_rate_config(mac.as_ptr(), &mut config) == 0 }
}

/// Set the Wi-Fi transmit power after ESP-NOW has borrowed the controller.
///
/// `WifiController::set_max_tx_power` is safe but requires `&mut self`; the node keeps
/// the sniffer and ESP-NOW handles borrowed for its whole life, so this is the narrow
/// direct IDF equivalent. The caller applies it only when adopting a fresh assignment.
///
/// The manager goes unused for the reason [`set_peer_rate`] takes one: IDF wants
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

/// The radio's maximum transmit power in quarter-dBm, as the IDF reports it now, or 0
/// when the read fails. The IDF floor is 8, so 0 cannot be a real reading.
///
/// What a heartbeat reports, read fresh each time: the value [`set_tx_power`] last
/// asked for is not evidence that the radio holds it. The manager is the same witness
/// it is there.
#[allow(
    unsafe_code,
    reason = "esp-radio requires a mutable controller after long-lived handles borrow it"
)]
pub fn tx_power(_manager: &EspNowManager<'_>) -> i8 {
    #[cfg(feature = "esp32c5")]
    use esp_wifi_sys_esp32c5::include as sys;
    #[cfg(feature = "esp32c6")]
    use esp_wifi_sys_esp32c6::include as sys;

    let mut power: i8 = 0;
    // SAFETY: Wi-Fi has started before the node enters its receive loop, and `power`
    // is a writable `i8` alive for the whole call. The function only reads the radio's
    // maximum transmit power. 0 is `ESP_OK`.
    if unsafe { sys::esp_wifi_get_max_tx_power(&mut power) } == 0 { power } else { 0 }
}

/// A plaintext station peer on whatever channel the radio is already using.
///
/// `channel: None` becomes 0, which ESP-NOW reads as "the current one" — the node
/// hops channels far more often than its core changes, so naming one explicitly
/// would mean re-registering the peer on every dwell.
const fn peer(mac: [u8; 6]) -> PeerInfo {
    PeerInfo {
        interface: EspNowWifiInterface::Station,
        peer_address: mac,
        // wartui does no encrypted ESP-NOW at all, so there is no PMK and no LMK.
        lmk: None,
        channel: None,
        encrypt: false,
    }
}

/// Make `mac` the node's one core peer, saying whether it registered.
///
/// `old` is the peer this replaces, if any, and is removed only after `mac` is
/// registered — never `BROADCAST`, which `esp-radio` registers at init and this
/// firmware never touches, because heartbeats keep using it after the core
/// changes. Registering before removing means a failure here leaves the previous
/// core's peer entry intact, so the caller's "leave `core` unchanged" is still
/// backed by a peer that works. A rate that refused to set still leaves the peer
/// usable, just slower, so only registration failure is reported to the caller.
///
/// `PeerExists` is not that failure: `old`'s removal above is best-effort, so a
/// node switching back to a core it held before can find it still registered.
/// Treated as success, same as `transmit` in `firmware/bridge/src/main.rs` — the
/// alternative is a node that can never move back onto a MAC it once left.
pub fn set_core_peer(
    manager: &EspNowManager<'_>,
    old: Option<[u8; 6]>,
    mac: [u8; 6],
) -> Result<(), EspNowError> {
    match manager.add_peer(peer(mac)) {
        Ok(()) | Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
        Err(err) => return Err(err),
    }
    let _ = set_peer_rate(manager, &mac);
    if let Some(prev) = old {
        let _ = manager.remove_peer(&prev);
    }
    Ok(())
}

/// Broadcast a frame for whoever is listening on the control channel.
///
/// Heartbeats only: nothing acknowledges a broadcast, so the return value says only
/// that the radio transmitted it. A node's core is learned from an admin or clear
/// frame, and until one arrives a heartbeat is the only thing a node can send.
pub fn broadcast(sender: &mut EspNowSender<'_>, frame: &[u8]) -> bool {
    // `esp-radio` registers the broadcast peer at init, and the waiter's `Drop`
    // blocks anyway, so waiting costs nothing that walking away would save.
    sender.send(&BROADCAST, frame).is_ok_and(|waiter| waiter.wait().is_ok())
}

/// Unicast a frame to `dst`, true only once the MAC layer has acknowledged it.
///
/// Sighting batches go this way rather than broadcast, so the radio retries a
/// frame nobody heard instead of it being gone after one try. `wait` blocks for the
/// send callback, the same as [`broadcast`]; the difference is what `Ok` means for a
/// unicast destination, which `firmware/bridge/src/main.rs`'s `transmit` also relies
/// on: the callback status is the MAC ack, not just an enqueue.
pub fn unicast(sender: &mut EspNowSender<'_>, dst: &[u8; 6], frame: &[u8]) -> bool {
    sender.send(dst, frame).is_ok_and(|waiter| waiter.wait().is_ok())
}
