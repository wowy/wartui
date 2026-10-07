//! The timings of a node's sweep and its heartbeat cycle.

use crate::plan::ChannelSet;

/// How long a node listens on one channel before moving on.
///
/// A sniffing node needs a beacon interval rather than a scan's dwell budget: the
/// default interval is 102.4 ms, and anything shorter can miss an access point entirely.
pub const CHANNEL_DWELL_MS: u32 = 125;

/// How long a node holds the control channel after its heartbeat.
///
/// This is the window an assignment has to
/// land inside, and the reason the host sends one only in the moment after a
/// heartbeat. It is also, once per [`ASSIGNED_BEAT_MS`], time in which no channel
/// is swept.
///
/// 100 ms is about five times the slowest the host answered across an hour of
/// captured traffic, and a window missed costs one heartbeat interval rather than
/// the assignment, because the node stays dirty and its next heartbeat re-sends
/// (`docs/phase-4-findings.md`, "The admin window, from a captured hour").
pub const ADMIN_WAIT_MS: u32 = 100;

/// How long an unassigned node waits between heartbeats.
///
/// A node told nothing parks on the control channel, so this is the whole of its
/// cycle rather than a slice: longer than [`ADMIN_WAIT_MS`], and short enough that
/// joining a fleet costs a second rather than a sweep.
pub const IDLE_BEAT_MS: u32 = 1000;

const _: () = assert!(
    IDLE_BEAT_MS >= ADMIN_WAIT_MS,
    "a parked node must hold the control channel for at least a full admin window"
);

/// How often an assigned node — sweeping or Bluetooth — sends a heartbeat and
/// holds [`ADMIN_WAIT_MS`] open.
///
/// On a timer rather than once per sweep: at any fleet size the window costs
/// about 2% of the node's time, against sniffing bounded only by the hop cost. The
/// price is latency — an assignment waits up to one interval for its window,
/// because a missed window costs one more interval before the host re-sends on
/// the next heartbeat. See `docs/duty-cycle-findings.md`.
pub const ASSIGNED_BEAT_MS: u32 = 5000;

const _: () = assert!(
    ASSIGNED_BEAT_MS >= ADMIN_WAIT_MS,
    "an assigned node must hold the control channel for at least a full admin window"
);

/// Where a node is in its sweep of the set it was assigned.
///
/// Arithmetic rather than radio, so it lives here and is checked by
/// `cargo test` rather than by a reflash. The thing it exists to get right is
/// the seam between adopting an assignment and dwelling on it: a node steps
/// this at the foot of every pass, *after* the dwell and the report, so a
/// cursor that started life sitting on the lowest index would be stepped past
/// before that channel was ever listened to. The lowest channel of every fresh
/// assignment would then go uncollected until the sweep came round — a hole
/// that reports as nothing at all, on the one channel most likely to be busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepCursor {
    at: Option<u8>,
}

impl SweepCursor {
    /// A cursor that has not begun, which is not the same as one on the first
    /// index. This is the state to put it in when an assignment is adopted.
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

    /// Step to the next index of `channels`, in ascending order, saying whether
    /// that wrapped — which is what a node counts as a completed sweep and
    /// answers with a heartbeat.
    ///
    /// The set is passed in rather than held because the assignment owns it: one
    /// copy means the two cannot disagree about which indices exist.
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
