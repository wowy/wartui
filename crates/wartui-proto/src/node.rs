//! The timings of a node's sweep and its heartbeat cycle.

use crate::plan::ChannelSet;

/// How long a node listens on one channel before moving on.
///
/// A sniffing node needs a whole beacon interval on each channel. The default interval
/// is 102.4 ms, and a shorter dwell can miss an access point entirely.
pub const CHANNEL_DWELL_MS: u32 = 125;

/// How long a node holds the control channel after its heartbeat.
///
/// An assignment must land inside this window, so the host sends one only just after a
/// heartbeat. Once per [`ASSIGNED_BEAT_MS`], it is also time in which no channel is
/// swept.
///
/// 100 ms is about five times the slowest the host answered across an hour of captured
/// traffic (`docs/phase-4-findings.md`, "The admin window, from a captured hour"). A
/// missed window costs one heartbeat interval, not the assignment: the node stays dirty,
/// and the host re-sends on its next heartbeat.
pub const ADMIN_WAIT_MS: u32 = 100;

/// How long an unassigned node waits between heartbeats.
///
/// A node told nothing parks on the control channel, so this is its whole cycle. It is
/// longer than [`ADMIN_WAIT_MS`], and short enough that joining a fleet costs a second.
pub const IDLE_BEAT_MS: u32 = 1000;

const _: () = assert!(
    IDLE_BEAT_MS >= ADMIN_WAIT_MS,
    "a parked node must hold the control channel for at least a full admin window"
);

/// How often an assigned node, sweeping or scanning Bluetooth, sends a heartbeat and
/// holds [`ADMIN_WAIT_MS`] open.
///
/// On a timer rather than once per sweep, so the window costs about 2% of the node's
/// time at any fleet size. The price is latency: an assignment waits up to one interval
/// for its window, and a missed window costs one more (`docs/duty-cycle-findings.md`).
pub const ASSIGNED_BEAT_MS: u32 = 5000;

const _: () = assert!(
    ASSIGNED_BEAT_MS >= ADMIN_WAIT_MS,
    "an assigned node must hold the control channel for at least a full admin window"
);

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
