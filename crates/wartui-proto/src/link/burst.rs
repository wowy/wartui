//! Deciding how long the bridge stays on the USB link while a frame is crossing it.
//!
//! The bridge firmware acts on this. The crate docs say why it lives here.
//!
//! ## The cost this exists for
//!
//! The USB Serial/JTAG endpoint holds one 64-byte packet each way. A pass of the bridge's
//! loop that moves one packet and then goes on to the radio, the stall check and an idle
//! wait leaves the next packet waiting for the pass after: the host's next OUT packet is
//! NAKed until the FIFO empties, and the next IN packet waits for the FIFO-empty wake. A
//! 1 KB frame is about seventeen packets, so it crosses in seventeen passes rather than at
//! USB speed. `docs/usb-link-findings.md` has the measurements.
//!
//! ## What is decided
//!
//! While a frame is part-way across in either direction, the pass polls both FIFOs
//! tightly. It gives the pass back when no frame is mid-transfer, when nothing has moved
//! for [`BURST_GAP_US`], or when [`BURST_BUDGET_US`] has passed since the burst began.
//! The gap ends a burst against a host that has stopped writing or reading. The budget
//! ends one against a host that never stops, so the radio's receive queue is still
//! drained between bursts.

/// The longest a burst waits for the next byte to move before giving the pass back.
///
/// Longer than the gap between packets from a host still writing a frame, or still
/// reading one: a full-speed bulk packet is about 50 µs on the bus. Beyond it the host has
/// stopped, and spinning on is a core burned for nothing.
pub const BURST_GAP_US: u64 = 200;

/// The longest a burst runs, however much is moving.
///
/// About one 1 KB frame at USB speed. The bound is the radio: `esp-radio`'s receive queue
/// is ten frames deep and drops its oldest, and the burst does not drain it. A burst this
/// long lets at most a few frames arrive against it.
pub const BURST_BUDGET_US: u64 = 1_000;

/// One burst on the link, timed against a microsecond clock that never goes backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkBurst {
    /// When the burst began.
    started: u64,
    /// When a byte last moved, in either direction.
    last_moved: u64,
}

impl LinkBurst {
    /// A burst beginning at `now_us`, counted as a moment something moved.
    #[must_use]
    pub const fn start(now_us: u64) -> Self {
        Self { started: now_us, last_moved: now_us }
    }

    /// Account for one poll of the link, and say whether to poll again.
    ///
    /// `moved` is whether that poll read or wrote any byte. `mid_frame` is whether a frame
    /// is part-way across in either direction. `false` means give the pass back.
    pub const fn keep_going(&mut self, now_us: u64, moved: bool, mid_frame: bool) -> bool {
        if moved {
            self.last_moved = now_us;
        }
        mid_frame
            && now_us.saturating_sub(self.last_moved) < BURST_GAP_US
            && now_us.saturating_sub(self.started) < BURST_BUDGET_US
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_burst_stops_when_no_frame_is_mid_transfer() {
        let mut burst = LinkBurst::start(0);
        assert!(!burst.keep_going(1, true, false));
    }

    #[test]
    fn link_burst_continues_when_frame_is_mid_transfer_at_start() {
        // The first poll has moved nothing yet, and starting counts as a move.
        let mut burst = LinkBurst::start(0);
        assert!(burst.keep_going(0, false, true));
    }

    #[test]
    fn link_burst_stops_when_nothing_moves_for_the_gap() {
        // A host that stopped reading mid-frame: nothing moves after the start.
        let mut burst = LinkBurst::start(0);
        assert!(burst.keep_going(BURST_GAP_US - 1, false, true));
        assert!(!burst.keep_going(BURST_GAP_US, false, true));
    }

    #[test]
    fn link_burst_continues_when_bytes_keep_moving() {
        // A packet every 150 µs, polled every 10 µs in between: inside the gap, so the
        // burst runs until the budget.
        let mut burst = LinkBurst::start(0);
        for now in (0..BURST_BUDGET_US).step_by(10) {
            assert!(burst.keep_going(now, now % 150 == 0, true), "stopped {now} µs in");
        }
    }

    #[test]
    fn link_burst_times_gap_from_last_move_when_bytes_moved_late() {
        let mut burst = LinkBurst::start(0);
        assert!(burst.keep_going(150, true, true));
        assert!(burst.keep_going(150 + BURST_GAP_US - 1, false, true));
        assert!(!burst.keep_going(150 + BURST_GAP_US, false, true));
    }

    #[test]
    fn link_burst_stops_when_budget_is_spent_despite_progress() {
        // A host that never stops sending: the radio still gets the pass back.
        let mut burst = LinkBurst::start(0);
        assert!(burst.keep_going(BURST_BUDGET_US - 1, true, true));
        assert!(!burst.keep_going(BURST_BUDGET_US, true, true));
    }

    #[test]
    fn link_burst_times_budget_from_start_when_clock_starts_late() {
        let start = 5_000_000;
        let mut burst = LinkBurst::start(start);
        assert!(burst.keep_going(start + BURST_BUDGET_US - 1, true, true));
        assert!(!burst.keep_going(start + BURST_BUDGET_US, true, true));
    }
}
