//! The promiscuous receive path.
//!
//! Everything here runs in the Wi-Fi driver's own task. The frame buffer it
//! hands us belongs to the driver and is gone the moment the callback returns,
//! and there is no way to pass state in: `Sniffer::set_receive_cb` takes a bare
//! `fn` pointer. So the callback parses what it needs into a fixed-size
//! [`Sighting`] and leaves it in a `static` ring for the main loop to drain.
//!
//! It also deduplicates against what is already pending. An access point beacons
//! roughly ten times in a 125 ms dwell, so without that the ring would fill with
//! copies of whichever network is loudest and drop the ones we have not seen —
//! exactly backwards. Collapsed to one entry each, forty-eight slots is more
//! than a single channel produces.
//!
//! Capture is opened and closed around the dwell rather than left running,
//! because promiscuous mode is on across every channel change — that is the
//! whole of `radio::park`. A frame arriving inside one of those toggles was
//! heard on a channel the ring is not stamped for: on the way out, control
//! traffic would be filed under the dwell channel, and on the way back, the
//! dwell channel's stragglers under control. Either way an access point ends up
//! named against a frequency it was never on, which is the one thing a fleet's
//! channel assignments are checked by. Frames in those windows are dropped.

use esp_radio::wifi::sniffer::PromiscuousPkt;
use esp_sync::NonReentrantMutex;
use wartui_proto::beacon::{Sighting, is_report, parse_mgmt};

/// How many distinct access points one dwell can hold.
const PENDING: usize = 48;

/// The frame check sequence, which the driver counts in `sig_len` but which is
/// not part of the frame. Four bytes of CRC read as a trailing information
/// element would be nonsense at best and a plausible-looking channel at worst.
const FCS_LEN: usize = 4;

struct Pending {
    items: [Option<Sighting>; PENDING],
    len: usize,
    taken: usize,
    /// Whether the radio is settled on [`Pending::channel`]. False through
    /// every channel change, and false from boot until the first dwell.
    armed: bool,
    /// The channel the radio is parked on, for frames that name none.
    channel: u8,
    /// Access points lost to a full ring, since boot.
    dropped: u32,
}

static PENDING_RING: NonReentrantMutex<Pending> = NonReentrantMutex::new(Pending {
    items: [const { None }; PENDING],
    len: 0,
    taken: 0,
    armed: false,
    channel: 0,
    dropped: 0,
});

/// Start collecting on `channel`, discarding anything left from the last one.
///
/// Call this once the radio is settled, not before the hop: a sighting the main
/// loop did not get to belongs to a channel the radio has already left, and one
/// arriving mid-hop belongs to neither.
pub fn arm(channel: u8) {
    PENDING_RING.with(|pending| {
        for slot in pending.items[..pending.len].iter_mut() {
            *slot = None;
        }
        pending.len = 0;
        pending.taken = 0;
        pending.channel = channel;
        pending.armed = true;
    });
}

/// Stop collecting, keeping what the dwell found.
///
/// The counterpart to [`arm`], called before leaving the channel. It does not
/// clear: everything pending was heard on a channel the radio was genuinely
/// parked on, and the whole point of the hop back to control is to report it.
pub fn disarm() {
    PENDING_RING.with(|pending| pending.armed = false);
}

/// The callback itself. Registered once, at boot.
pub fn on_frame(pkt: PromiscuousPkt<'_>) {
    let frame = pkt.data.get(..pkt.data.len().saturating_sub(FCS_LEN)).unwrap_or(pkt.data);

    // Before the lock, not after. Promiscuous mode delivers every frame on the
    // channel and almost all of them are data; taking a critical section
    // thousands of times a second to decide that would be the most expensive
    // thing this firmware does.
    if !is_report(frame) {
        return;
    }

    PENDING_RING.with(|pending| {
        // Mid-hop, or before the first dwell. There is no channel to file this
        // under that would be true.
        if !pending.armed {
            return;
        }

        // `rx_ctrl.rssi` is a signed 8-bit bitfield, but the generated accessor
        // extracts the bits unsigned and widens, so -62 dBm arrives as 194 and
        // any clamp saturates every frame to 127. Reinterpreting the low byte
        // recovers it, and keeps working unchanged if the binding is ever fixed
        // to sign-extend. Same trap the bridge documents on its receive path.
        #[allow(clippy::cast_possible_truncation)]
        let rssi = (pkt.rx_cntl.rssi as u8) as i8;

        let Some(sighting) = parse_mgmt(frame, rssi, pending.channel) else { return };

        if pending.items[..pending.len].iter().flatten().any(|s| s.bssid == sighting.bssid) {
            return;
        }
        if pending.len == PENDING {
            // Dropping the newest rather than the oldest. Everything already
            // here is a distinct access point that has not been reported yet;
            // evicting one to make room would trade a certain observation for
            // a possible one.
            pending.dropped = pending.dropped.wrapping_add(1);
            return;
        }
        pending.items[pending.len] = Some(sighting);
        pending.len += 1;
    });
}

/// Take the oldest sighting not yet reported, if there is one.
///
/// One at a time rather than a bulk drain, so the caller never holds a several
/// kilobyte buffer on the stack and the lock is never held across a transmit.
pub fn take() -> Option<Sighting> {
    PENDING_RING.with(|pending| {
        let slot = pending.items.get_mut(pending.taken)?;
        let sighting = slot.take()?;
        pending.taken += 1;
        Some(sighting)
    })
}

/// Access points lost to a full ring since boot. A number that climbs means
/// [`PENDING`] is too small for the neighbourhood, not that the dwell is wrong.
pub fn dropped() -> u32 {
    PENDING_RING.with(|pending| pending.dropped)
}
