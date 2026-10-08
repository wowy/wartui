//! The promiscuous receive path.
//!
//! Everything here runs in the Wi-Fi driver's own task, on a buffer that dies when
//! the callback returns, with no way to pass state in — `set_receive_cb` takes a bare
//! `fn` pointer. So the callback parses into a fixed-size [`Sighting`] and leaves it
//! in a `static` buffer for the main loop.
//!
//! It also deduplicates against what is already pending: an access point beacons about
//! ten times in a 125 ms dwell, so without that the buffer fills with copies of the
//! loudest network and drops the ones not yet seen. That lookup runs once per beacon
//! with interrupts held off, so [`WifiPending`] finds a BSSID by hash rather than by
//! scanning; the `wartui_proto::pending` docs' "Why the lookup is hashed" has the timings.
//!
//! And it deduplicates against [`SEEN`], the dedup ring itself: an access point already
//! reported and not yet due to be reported again is worth nothing, but without this
//! check it takes a [`PENDING`] slot, and a dense channel fills that buffer with
//! addresses `report` would go on to discard anyway — starving the access points not
//! yet seen this dwell. `SEEN` is asked only for a BSSID not already pending, which
//! spares the lookup on every repeat beacon and changes nothing: a pending BSSID is
//! ignored whatever `SEEN` would say. It is asked from inside [`DWELL`]'s lock,
//! which is the one place the two nest, and `ble::Scanner::sweep` nests it the same way
//! inside `ble::PENDING`; every other caller takes `SEEN` on its own, per sighting, and
//! never across a transmit.
//!
//! An access point that finds the buffer full is counted in
//! [`Refused`](wartui_proto::pending::Refused), once per dwell however often it beacons,
//! so [`dropped`] counts addresses rather than packets. It is not in [`SEEN`], so the
//! next dwell reports it: a refusal is mostly delay.
//!
//! Capture is opened and closed around the dwell rather than left running, because
//! promiscuous mode is on across every channel change (`radio::park`). A frame
//! arriving inside one of those toggles was heard on a channel the buffer is not stamped
//! for — control traffic filed under the dwell channel on the way out, the dwell
//! channel's stragglers under control on the way back — so an access point would end
//! up named against a frequency it was never on, which is the one thing a fleet's
//! channel assignments are checked by. Frames in those windows are dropped.

use esp_hal::time::Instant;
use esp_radio::wifi::sniffer::PromiscuousPkt;
use esp_sync::NonReentrantMutex;
use wartui_proto::beacon::{Sighting, is_report, parse_mgmt};
#[cfg(feature = "esp32c5")]
use wartui_proto::dedup::C5DedupRing as ChipDedupRing;
#[cfg(feature = "esp32c6")]
use wartui_proto::dedup::C6DedupRing as ChipDedupRing;
use wartui_proto::pending::WifiPending;

/// The dedup ring: addresses already reported and not yet due again.
///
/// Sized per chip: 512 addresses on a C5, 4,096 on a C6, which has the RAM and is
/// the likely Bluetooth node (`wartui_proto::dedup`, "Why the ring is hashed").
///
/// A `static` rather than a field of `Node`, because the receive callback needs to
/// reach it and cannot borrow anything (see the module doc). Nests inside
/// [`DWELL`]'s lock in [`on_frame`] and inside `ble::PENDING`'s in
/// `ble::Scanner::sweep`; every other caller in `main.rs` takes it on its own, per
/// sighting, and that lock order must hold everywhere `SEEN` is used.
pub static SEEN: NonReentrantMutex<ChipDedupRing> = NonReentrantMutex::new(ChipDedupRing::new());

/// Milliseconds since boot, as [`SEEN`] counts them. Truncation is the ring's own wrap.
#[allow(clippy::cast_possible_truncation)]
pub fn now_ms() -> u32 {
    Instant::now().duration_since_epoch().as_millis() as u32
}

/// How many distinct access points one dwell can hold.
///
/// Sized for a dense channel rather than a quiet one: overflow turns the newest access
/// point away until a later dwell, and the hashed lookup costs the same at any size, so
/// the price of headroom is 61 bytes of static RAM a slot. At 64 the index is still 128
/// slots.
const PENDING: usize = 64;

/// Hash slots behind [`PENDING`]: the smallest power of two at least twice it, as
/// [`WifiPending`] requires.
const PENDING_INDEX: usize = (2 * PENDING).next_power_of_two();

/// The frame check sequence, which the driver counts in `sig_len` but which is not part
/// of the frame: four bytes of CRC read as a trailing element would be nonsense at best
/// and a plausible-looking channel at worst.
const FCS_LEN: usize = 4;

/// What the dwell in progress has heard, and where it is listening.
struct Dwell {
    /// This dwell's sightings, one per BSSID.
    sightings: WifiPending<PENDING, PENDING_INDEX>,
    /// Whether the radio is settled on [`Dwell::channel`]. False through
    /// every channel change, and false from boot until the first dwell.
    armed: bool,
    /// The channel the radio is parked on, for frames that name none.
    channel: u8,
}

static DWELL: NonReentrantMutex<Dwell> =
    NonReentrantMutex::new(Dwell { sightings: WifiPending::new(), armed: false, channel: 0 });

/// Start collecting on `channel`, discarding anything left from the last one.
///
/// Call this once the radio is settled, not before the hop: a sighting the main loop
/// did not get to belongs to a channel already left, and one arriving mid-hop to
/// neither.
pub fn arm(channel: u8) {
    DWELL.with(|dwell| {
        dwell.sightings.clear();
        dwell.channel = channel;
        dwell.armed = true;
    });
}

/// Stop collecting, keeping what the dwell found.
///
/// The counterpart to [`arm`], called before leaving the channel. It does not clear:
/// everything pending was heard while the radio was genuinely parked.
pub fn disarm() {
    DWELL.with(|dwell| dwell.armed = false);
}

/// The callback itself. Registered once, at boot.
pub fn on_frame(pkt: PromiscuousPkt<'_>) {
    let frame = pkt.data.get(..pkt.data.len().saturating_sub(FCS_LEN)).unwrap_or(pkt.data);

    // Before the lock, not after: promiscuous mode delivers every frame on the
    // channel and almost all are data, so a critical section thousands of times a
    // second to decide that would be the most expensive thing here.
    if !is_report(frame) {
        return;
    }

    DWELL.with(|dwell| {
        // Mid-hop, or before the first dwell. There is no channel to file this
        // under that would be true.
        if !dwell.armed {
            return;
        }

        // The signed-bitfield trap the bridge documents on its own receive path:
        // the accessor widens unsigned, so the low byte has to be reinterpreted.
        #[allow(clippy::cast_possible_truncation)]
        let rssi = (pkt.rx_cntl.rssi as u8) as i8;

        let Some(sighting) = parse_mgmt(frame, rssi, dwell.channel) else { return };

        // Already reported and not due again: a pending slot spent on it is one taken
        // from an access point not yet seen this dwell. Asked only for a BSSID not
        // already pending; see the module doc for why that order is safe.
        dwell.sightings.record(sighting, |s| {
            let now = now_ms();
            SEEN.with(|seen| seen.is_due(&s.bssid, Some(rssi), now))
        });
    });
}

/// Take the oldest sighting not yet reported, if there is one.
///
/// One at a time rather than a bulk drain, so nothing holds a several-kilobyte buffer
/// on the stack and the lock is never held across a transmit.
pub fn take() -> Option<Sighting> {
    DWELL.with(|dwell| dwell.sightings.take())
}

/// Access points turned away by a full buffer since boot, each counted once per dwell.
/// Wraps. The heartbeat carries it to the host. A number that climbs means [`PENDING`]
/// is too small for the neighborhood, not that the dwell is wrong.
pub fn dropped() -> u16 {
    DWELL.with(|dwell| dwell.sightings.dropped())
}
