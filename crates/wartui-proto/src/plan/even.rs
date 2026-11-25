//! The even deal: one order, cycled, so shares differ by at most one channel.

use super::{ChannelSet, Dealer, POPULAR};

/// The even deal.
///
/// The order is channels 1, 6 and 11, those the set holds, then the rest ascending. Position `p`
/// goes to share `p mod n`. The deal runs for as many positions as there are channels or shares,
/// whichever is more, starting the order over when it runs out.
///
/// - With three or more shares no share holds two of 1, 6 and 11.
/// - With at least as many channels as shares, each channel is dealt once and shares differ by at
///   most one, so sweep times stay within one dwell of each other.
/// - With fewer, every share holds one channel and 1, 6 and 11 are the first held twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Even;

impl Dealer for Even {
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]) {
        let n = shares.len();
        if n == 0 {
            return;
        }
        let popular = POPULAR.into_iter().filter(move |idx| channels.contains(*idx));
        let rest = channels.indices().filter(|idx| !POPULAR.contains(idx));
        // `len` is at most `NUM_SCAN_CHANNELS`.
        let positions = (channels.len() as usize).max(n);
        for (p, idx) in popular.chain(rest).cycle().take(positions).enumerate() {
            shares[p % n].insert(idx);
        }
    }
}
