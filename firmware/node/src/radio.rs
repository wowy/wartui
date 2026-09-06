//! The two things this firmware asks of the radio.
//!
//! Parking is not `esp_wifi_set_channel` alone. The vendor firmware wraps it in
//! a promiscuous-mode toggle (`setFixedChannel`, `src/WiFiOps.cpp:586-620`)
//! because on an unassociated station interface the channel does not otherwise
//! stick. A node that sniffs has promiscuous mode on for its own reasons, so
//! that workaround falls out for free on the dwell side; the hop back to the
//! control channel is the one that has to be checked on hardware.

use esp_radio::esp_now::{EspNowManager, EspNowSender};
use esp_radio::wifi::sniffer::Sniffer;
use wartui_proto::link::BROADCAST;

/// Tune the radio, saying whether it took.
///
/// `listen` decides whether promiscuous mode is left on afterwards: on for a
/// dwell, off on the control channel, where every data frame in the room would
/// otherwise reach the callback for nothing.
///
/// Promiscuous mode is switched on across the change either way. That is the
/// vendor's `setFixedChannel` (`src/WiFiOps.cpp:600-618`) and it is not
/// decoration — on an unassociated station interface the channel does not stick
/// without it, and a node whose channel silently did not change would transmit
/// its observations into the wrong room and hear no assignment at all.
///
/// A refusal is worth reporting rather than retrying. It is the blob declining
/// a channel outright, and both known causes are settled before the first sweep
/// and unchanged by trying again: the band mode was not set and a 5 GHz channel
/// was asked for, or the channel is outside the configured regulatory domain.
/// The second cost a bench session — `esp-radio` defaults `country_info` to
/// `CN` under a manual policy, which permits 36-64 and 149-165 and refuses the
/// whole of 100-144. `main` sets `US` for that reason.
pub fn park(manager: &EspNowManager<'_>, sniffer: &Sniffer<'_>, channel: u8, listen: bool) -> bool {
    let _ = sniffer.set_promiscuous_mode(true);
    let parked = manager.set_channel(channel).is_ok();
    if !listen {
        let _ = sniffer.set_promiscuous_mode(false);
    }
    parked
}

/// Put a frame on the air for whoever is listening.
///
/// Broadcast, because that is how a plaintext node reaches its core
/// (`sendHeartbeat`, `src/WiFiOps.cpp:876-878`) and because it needs no peer
/// slot on either end. Nothing acknowledges a broadcast, so the return value
/// says only that the radio accepted and transmitted it — the ack that matters
/// runs the other way, when the host unicasts an assignment and this node's MAC
/// hardware answers it.
pub fn broadcast(sender: &mut EspNowSender<'_>, frame: &[u8]) -> bool {
    // `esp-radio` registers the broadcast peer at init, so there is nothing to
    // add here. The waiter's `Drop` blocks anyway, so waiting is not a cost we
    // could avoid by walking away.
    sender.send(&BROADCAST, frame).is_ok_and(|waiter| waiter.wait().is_ok())
}
