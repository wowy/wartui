//! What this firmware asks of the Wi-Fi radio: the channel, and promiscuous mode around it.
//!
//! The node sends no Wi-Fi frames. Everything it says goes over the node bus (`bus`), so the
//! radio only listens, and this module is where it is tuned.

use esp_radio::wifi::sniffer::Sniffer;
use esp_radio::wifi::{SecondaryChannel, WifiController};

/// Tune the radio for a dwell, saying whether it took. Promiscuous mode is left on.
///
/// Promiscuous mode is switched on across the change, and it is not decoration: on an
/// unassociated station interface the channel does not stick without it, and a node whose
/// channel silently did not change reports the right networks against the wrong frequency.
///
/// A refusal is worth reporting rather than retrying: it is the blob declining a channel
/// outright, and both known causes — an unset band mode, or a channel outside the regulatory
/// domain — are settled before the first sweep.
pub fn park(controller: &mut WifiController<'_>, sniffer: &Sniffer, channel: u8) -> bool {
    let _ = sniffer.set_promiscuous_mode(true);
    controller.set_channel(channel, SecondaryChannel::None).is_ok()
}

/// Switch promiscuous mode off after a dwell, so the frames on the channel stop reaching the
/// callback while nothing is collecting them.
pub fn quiet(sniffer: &Sniffer) {
    let _ = sniffer.set_promiscuous_mode(false);
}
