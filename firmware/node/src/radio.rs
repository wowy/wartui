//! The two things this firmware asks of the radio.
//!
//! Parking is not `set_channel` alone — see [`park`]. A node that sniffs has
//! promiscuous mode on for its own reasons, so that falls out for free on the dwell
//! side; the hop back to the control channel is the one to check on hardware.

use esp_radio::esp_now::{EspNowManager, EspNowSender};
use esp_radio::wifi::sniffer::Sniffer;
use wartui_proto::link::BROADCAST;

/// Tune the radio, saying whether it took.
///
/// `listen` decides whether promiscuous mode is left on afterwards: on for a
/// dwell, off on the control channel, where every data frame in the room would
/// otherwise reach the callback for nothing.
///
/// Promiscuous mode is switched on across the change either way. That is the vendor's
/// `setFixedChannel` (`src/WiFiOps.cpp:600-618`) and it is not decoration: on an
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
