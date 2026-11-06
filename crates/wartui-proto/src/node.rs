//! A node's sweep and heartbeat timings, and the cursor that steps it through its assigned
//! channels.

use crate::plan::ChannelSet;

/// How long a node listens on one channel before moving on.
///
/// A sniffing node needs a whole beacon interval on each channel. The default interval
/// is 102.4 ms, and a shorter dwell can miss an access point entirely.
pub const CHANNEL_DWELL_MS: u32 = 125;

/// How often an unassigned node sends a heartbeat.
///
/// An unassigned node does nothing else, so this is its whole cycle, and short enough that
/// joining a fleet costs a second.
pub const IDLE_BEAT_MS: u32 = 1000;

/// How often an assigned node, sweeping or scanning Bluetooth, sends a heartbeat.
///
/// On a timer rather than once per sweep, so the heartbeat's cost is the same at any fleet
/// size. The host reads liveness and the epoch held from it, so it is also how long the host
/// may take to see an assignment adopted.
pub const ASSIGNED_BEAT_MS: u32 = 5000;

/// Where a node is in its sweep of the set it was assigned.
///
/// It exists to get one seam right: adopting an assignment and dwelling on it. A node
/// steps the cursor at the foot of every pass, *after* the dwell and the report. A
/// cursor that started on the lowest index would be stepped past before that channel
/// was heard. The lowest channel of every fresh assignment would then go uncollected
/// until the sweep came round. That hole reports as nothing, on the channel most likely
/// to be busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepCursor {
    at: Option<u8>,
}

impl SweepCursor {
    /// A cursor that has not begun, which differs from one on the first index. Put it
    /// in this state when an assignment is adopted.
    #[must_use]
    pub const fn new() -> Self {
        Self { at: None }
    }

    /// The [`SCAN_CHANNELS`] index being dwelt on, or `None` before the first
    /// step of a sweep.
    ///
    /// [`SCAN_CHANNELS`]: crate::plan::SCAN_CHANNELS
    #[must_use]
    pub const fn index(self) -> Option<u8> {
        self.at
    }

    /// Step to the next index of `channels` in ascending order, and say whether that
    /// wrapped. A wrap is what a node counts as a completed sweep.
    ///
    /// The set is passed in rather than held, because the assignment owns it. With one
    /// copy, the two cannot disagree about which indices exist.
    pub fn advance(&mut self, channels: ChannelSet) -> bool {
        let next = match self.at {
            Some(at) => channels.indices().find(|idx| *idx > at),
            None => channels.first(),
        };
        match next {
            Some(idx) => {
                self.at = Some(idx);
                false
            }
            None => {
                self.at = channels.first();
                true
            }
        }
    }
}
