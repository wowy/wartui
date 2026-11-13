//! Spike only: the checks a phone host would make on the wartui Bluetooth link, kept apart from
//! the Bluetooth stack so they run under `cargo test` without one.
//!
//! An [`Attachment`] is one subscription's view of the Up stream: what decoded, in what order,
//! and what the load generator's seq and CRC say was lost or damaged. [`Rtt`] times `GetStatus`
//! one at a time, [`Keepalive`] holds UART0's host gate open beside them, [`Beats`] tallies node
//! heartbeats per slot, and [`Batches`] sighting batches and their losses. [`Reconcile`] sets the
//! run's seq gaps against what the P4 says it dropped, and [`bridge`] the P4's own counters across
//! the run. [`events`] stamps what happened, as it happened, and [`secure`] holds a run to an
//! encrypted, bonded link. `main.rs` owns the radio and the serial port.

use wartui_proto::link::{HostToBridge, MAX_HOST_FRAME, encode_host_frame};

pub mod attachment;
pub mod batches;
pub mod beats;
pub mod bridge;
pub mod events;
pub mod reconcile;
pub mod rtt;
pub mod secure;

pub use attachment::{Attachment, AttachmentSummary, Rates, Seen, SeqCheck, SeqGap};
pub use batches::{BatchGap, Batches, SlotBatches};
pub use beats::{BeatGap, Beats, SlotBeats};
pub use bridge::{CounterDelta, FleetSummary};
pub use events::{Event, EventKind};
pub use reconcile::Reconcile;
pub use rtt::{Keepalive, Rtt, RttSummary, StatusAsk};
pub use secure::{Secure, SecureWait};

/// `msg` as the frame a central writes to Down.
///
/// # Panics
/// Never for a message this build encodes: each fits [`MAX_HOST_FRAME`].
#[must_use]
pub fn host_frame(msg: &HostToBridge) -> Vec<u8> {
    let mut out = vec![0u8; MAX_HOST_FRAME];
    let len = encode_host_frame(msg, &mut out).expect("a host frame fits MAX_HOST_FRAME");
    out.truncate(len);
    out
}

/// Bytes as lowercase hex, two digits each, unspaced: what nRF Connect's write field takes.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use wartui_proto::link::{FrameAccumulator, decode_frame};
    use wartui_proto::spike::SpikeProfile;

    use super::*;

    #[test]
    fn host_frame_decodes_as_sent_when_fed_back() {
        let msg = HostToBridge::SpikeLoad { profile: SpikeProfile::Burst, count: 2_000 };
        let frame = host_frame(&msg);
        assert_eq!(frame.last(), Some(&0), "a frame ends in its terminator");
        let mut acc = FrameAccumulator::<MAX_HOST_FRAME>::new();
        let mut body = frame.iter().find_map(|&b| acc.push(b).map(|f| f.to_vec())).unwrap();
        assert_eq!(decode_frame::<HostToBridge>(&mut body), Ok(msg));
    }

    #[test]
    fn hex_writes_two_digits_when_byte_is_small() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
    }
}
