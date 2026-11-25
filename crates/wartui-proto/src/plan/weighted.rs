//! The weighted deal: the quiet 5 GHz channels go to a few sparse nodes, so the rest sweep
//! shorter and visit the busy channels more often.
//!
//! Two drives measured 43% of the fleet's radio time on quiet 5 GHz channels that gave about 2%
//! of the networks (issue #362). Weighting spends that time on the busy channels instead. Whether
//! it catches more is measured by a comparison drive, so it is a planner beside the even deal, not
//! a replacement.
//!
//! The rules, over `channels` and `n` shares:
//!
//! 1. **Busy list**: 1, 6 and 11, those held, the rest of 2.4 GHz ascending, then
//!    [`BUSY_FIVE_GHZ`] in its order, those held. **Quiet list**: the rest of the set, ascending.
//! 2. **Sparse nodes** `k`: [`SparseNodes::count_for`], clamped to `n - 3` and to the quiet list's
//!    length, so three busy shares always remain for 1, 6 and 11 and every sparse node holds a
//!    channel.
//! 3. **The even deal instead** when `n < 4`, when `k` is 0, or when the busy list is shorter than
//!    the `n - k` busy shares. So a short list, or one with no quiet channel, deals as under
//!    [`Even`], and every share holds a channel.
//! 4. **Sparse nodes are the last `k` shares**, the highest Wi-Fi slots, away from the first
//!    three, where 1, 6 and 11 land. Quiet position `p` goes to sparse node `p mod k`.
//! 5. **Busy shares are the first `b = n - k`.** Busy position `p` goes to share `p` while
//!    `p < b`, and to share `(3 + (p - b)) mod b` after. Every lap after the first starts at the
//!    fourth share, so busy shares differ by at most one and a share holding 1, 6 or 11 is never
//!    longer than one holding none of them.
//!
//! The busy list holds 2.4 GHz before 5 GHz, so with at least `b` busy channels in each band
//! every busy share holds both, and losing a busy node blinds the fleet to neither band. A sparse
//! node holds 5 GHz only, and losing one loses quiet channels until the re-cut.

use super::{
    BUSY_FIVE_GHZ, ChannelSet, Dealer, Even, NUM_SCAN_CHANNELS, POPULAR, SparseNodes, is_five_ghz,
    is_quiet,
};

/// The weighted deal. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Weighted {
    /// How many sparse nodes to ask for, before the clamp.
    pub sparse_nodes: SparseNodes,
}

impl Dealer for Weighted {
    fn deal(&self, channels: ChannelSet, shares: &mut [ChannelSet]) {
        let n = shares.len();
        if n < 4 {
            Even.deal(channels, shares);
            return;
        }
        let mut busy = Order::new();
        for idx in POPULAR {
            if channels.contains(idx) {
                busy.push(idx);
            }
        }
        for idx in channels.indices().filter(|idx| !is_five_ghz(*idx) && !POPULAR.contains(idx)) {
            busy.push(idx);
        }
        for idx in BUSY_FIVE_GHZ {
            if channels.contains(idx) {
                busy.push(idx);
            }
        }
        let mut quiet = Order::new();
        for idx in channels.indices().filter(|idx| is_quiet(*idx)) {
            quiet.push(idx);
        }

        let k = self.sparse_nodes.count_for(n).min(n - 3).min(quiet.len);
        let b = n - k;
        if k == 0 || busy.len < b {
            Even.deal(channels, shares);
            return;
        }
        for (p, idx) in quiet.iter().enumerate() {
            shares[b + p % k].insert(idx);
        }
        for (p, idx) in busy.iter().enumerate() {
            let share = if p < b { p } else { (3 + (p - b)) % b };
            shares[share].insert(idx);
        }
    }
}

/// Channel indices in deal order, without allocating.
struct Order {
    indices: [u8; NUM_SCAN_CHANNELS as usize],
    len: usize,
}

impl Order {
    const fn new() -> Self {
        Self { indices: [0; NUM_SCAN_CHANNELS as usize], len: 0 }
    }

    /// Append `idx`. A set holds at most [`NUM_SCAN_CHANNELS`] indices, so there is room.
    fn push(&mut self, idx: u8) {
        if let Some(slot) = self.indices.get_mut(self.len) {
            *slot = idx;
            self.len += 1;
        }
    }

    fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        self.indices[..self.len].iter().copied()
    }
}
