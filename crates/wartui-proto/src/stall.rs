//! Deciding when the bridge's USB transmit endpoint has stopped draining.
//!
//! Only the firmware acts on this, and it lives here for the reason
//! [`crate::outbox`] does: a `no_std` binary cannot run a test. The rule below
//! shipped two defects, neither found by review — the first took a bridge on a
//! bench beside a talking fleet, the second an operator quitting a session and
//! watching the board reset three seconds later. Both are one line of arithmetic
//! against a clock, and both are now tests a few microseconds long.
//!
//! ## The failure this exists for
//!
//! The USB Serial/JTAG IN endpoint answers every write with "not now" and never
//! stops: `SERIAL_IN_EP_DATA_FREE` goes to zero when `WR_DONE` is set and, per the
//! TRM, comes back only "until data in UART Tx FIFO is read by USB Host". If that
//! read never lands the flag never clears and the device end cannot make it. The
//! receive path is untouched, so the bridge goes on decoding and executing
//! commands it cannot answer — from the host, a port that opens, writes that
//! succeed, and silence. Nothing subtler than a reset is available.
//!
//! ## What is actually being measured
//!
//! Not "how long since a byte moved", but how long the *contradiction* has lasted:
//! a host demonstrably asking and a transmit path demonstrably refusing. That
//! distinction is the whole of this module, and `docs/phase-3-findings.md` has the
//! bench measurement behind each clause.

/// How long the transmit path may refuse every byte while a host waits.
///
/// Chosen against the host's clock rather than the bridge's: `wartui` gives up
/// after six seconds (`crates/wartui-bridge/src/serial.rs`), so resetting at three
/// leaves room for the reboot and a fresh `Ready` to land inside that window. Also
/// far longer than any legitimate gap — the test is for *no* progress at all.
pub const TX_STALL_TIMEOUT_MS: u64 = 3_000;

/// How recently the host must have spoken to count as still being there.
///
/// Deliberately longer than the host's own five-second `status_interval`
/// (`crates/wartui-core/src/engine.rs`), because a connected host with a quiet
/// fleet says nothing in between. Below that interval, an established capture
/// reads as an absent host for two seconds in every five and no wedge is ever
/// noticed.
pub const HOST_PRESENT_WINDOW_MS: u64 = 10_000;

/// Whether the transmit endpoint has stopped draining while a host waited.
///
/// Fed one observation per pass of the bridge's main loop, and asked after each
/// one whether to give up. Four facts have to hold together, and no three of
/// them are enough:
///
/// - something is queued, so silence is not simply having nothing to say;
/// - nothing has moved for [`TX_STALL_TIMEOUT_MS`], so the endpoint is not
///   merely slow;
/// - a host frame decoded inside [`HOST_PRESENT_WINDOW_MS`], so there is a host
///   at all;
/// - and one decoded *since the stall began*, so that host is still there now
///   rather than having been there a moment ago.
///
/// # Why the clock starts at the contradiction
///
/// Timing from the last byte written looks equivalent and is not. A bridge left
/// powered beside a talkative fleet fills its rings with nobody reading, so by the
/// time an operator attaches the last byte moved *hours* ago — and all of it would
/// count against a transmit path with nothing wrong with it. The clock is set when
/// the contradiction begins, which the host-present clause is what holds back.
///
/// # Why the fourth clause is not a refinement of the third
///
/// A host that has *quit* satisfies "spoke inside the window" for a further
/// [`HOST_PRESENT_WINDOW_MS`], and quitting is exactly what stops the endpoint
/// draining — so with a fleet in earshot the rings fill the moment it lets go, and
/// since [`TX_STALL_TIMEOUT_MS`] is the shorter of the two the reset always won
/// that race. A host genuinely waiting keeps asking, so requiring a frame decoded
/// *after* the stall began separates the two at no cost to the real case. Strictly
/// greater than, for the same reason: a host whose last frame lands in the
/// millisecond the stall arms has not asked since, and `>=` restores the bug.
///
/// # Why there is no default host
///
/// Seeded with a time instead of `None`, this reads as a host present for the first
/// [`HOST_PRESENT_WINDOW_MS`] of *every* life — so a bridge powered beside a
/// talking fleet with nothing attached fills its rings, resets, and comes back into
/// the same window for ever. Presence is something a host demonstrates by sending a
/// frame, and there is no such thing as a default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StallWatch {
    /// When the transmit path first refused a byte with a host waiting, or
    /// `None` if it is not refusing them now.
    stall_since: Option<u64>,
    /// When a frame from the host last decoded, or `None` if none ever has.
    last_host: Option<u64>,
}

impl StallWatch {
    /// Nothing queued, nobody here, no stall in progress.
    #[must_use]
    pub const fn new() -> Self {
        Self { stall_since: None, last_host: None }
    }

    /// Record proof of a host: a frame that decoded, at `now_ms`.
    ///
    /// Only a frame that *decoded* may be reported: a board running node firmware
    /// talks constantly down the same wire and none of it is a frame.
    pub const fn note_host(&mut self, now_ms: u64) {
        self.last_host = Some(now_ms);
    }

    /// When a frame from the host last decoded, or `None` if none ever has.
    ///
    /// Exposed so that anything else needing to know whether a host is there reads the
    /// clock that is already kept rather than starting a second one. The bridge's panel
    /// does: it falls back to what it knows on its own when the host stops talking, and
    /// two clocks for one fact would disagree the moment either changed.
    #[must_use]
    pub const fn last_host(&self) -> Option<u64> {
        self.last_host
    }

    /// Account for one pass of the outbox, and say whether to give up.
    ///
    /// `moved` is whether that pass placed any byte at all; `queued` is whether
    /// anything is still waiting to go. `true` means the four clauses above all
    /// hold and the only remedy left is a reset.
    ///
    /// `now_ms` must never go backwards. The firmware reads it from the boot
    /// instant, which cannot, and a caller that breaks the rule is caught by the
    /// subtraction below rather than mistaken for a host that is present.
    pub fn note_tx(&mut self, moved: bool, queued: bool, now_ms: u64) -> bool {
        let host_here = self.last_host.is_some_and(|at| now_ms - at < HOST_PRESENT_WINDOW_MS);
        if moved || !queued || !host_here {
            self.stall_since = None;
            return false;
        }
        let since = *self.stall_since.get_or_insert(now_ms);
        // Still talking, not merely seen lately: this is the clause that tells
        // a wedged endpoint from an operator who has just pressed `q`.
        let still_asking = self.last_host.is_some_and(|at| at > since);
        still_asking && now_ms - since >= TX_STALL_TIMEOUT_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pass of the loop: the host spoke, then the pump moved nothing
    /// against a queue that is not empty. The wedge, in other words.
    fn wedged(watch: &mut StallWatch, now_ms: u64) -> bool {
        watch.note_host(now_ms);
        watch.note_tx(false, true, now_ms)
    }

    #[test]
    fn a_bridge_nobody_has_ever_spoken_to_is_never_reset() {
        let mut watch = StallWatch::new();
        // A bench bridge beside a talking fleet: rings full, endpoint refusing,
        // for an hour.
        for second in 0..3_600 {
            assert!(!watch.note_tx(false, true, second * 1_000));
        }
    }

    #[test]
    fn a_wedged_endpoint_with_the_host_still_asking_is_reset_at_the_timeout() {
        let mut watch = StallWatch::new();
        assert!(!wedged(&mut watch, 0));
        assert!(!wedged(&mut watch, TX_STALL_TIMEOUT_MS - 1));
        assert!(wedged(&mut watch, TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn a_host_that_quits_is_not_a_host_that_is_waiting() {
        let mut watch = StallWatch::new();
        // Its last frame arms the stall in the same pass, and it never speaks
        // again. `HOST_PRESENT_WINDOW_MS` goes on believing in it for another
        // seven seconds after the timeout would have fired, and the fourth
        // clause is the only thing standing in the way.
        assert!(!wedged(&mut watch, 0));
        for ms in 1..HOST_PRESENT_WINDOW_MS {
            assert!(!watch.note_tx(false, true, ms), "reset {ms} ms after the host quit");
        }
    }

    #[test]
    fn a_pass_that_moved_a_byte_starts_the_clock_again() {
        let mut watch = StallWatch::new();
        // A congested endpoint that keeps almost catching up: it refuses for
        // just under the timeout, lets one byte through, and does it again.
        // That is a slow host, not a wedged one, and it may go on for ever.
        for round in 0..100 {
            let base = round * TX_STALL_TIMEOUT_MS;
            assert!(!wedged(&mut watch, base));
            assert!(!wedged(&mut watch, base + TX_STALL_TIMEOUT_MS - 1));
            watch.note_host(base + TX_STALL_TIMEOUT_MS);
            assert!(!watch.note_tx(true, true, base + TX_STALL_TIMEOUT_MS));
        }
    }

    #[test]
    fn an_empty_outbox_is_not_a_stall() {
        let mut watch = StallWatch::new();
        // A quiet fleet and an attentive host: nothing to say, and saying it
        // for an hour is not evidence of anything.
        for second in 0..3_600 {
            watch.note_host(second * 1_000);
            assert!(!watch.note_tx(false, false, second * 1_000));
        }
    }

    #[test]
    fn the_clock_starts_when_the_host_arrives_and_not_when_the_rings_filled() {
        let mut watch = StallWatch::new();
        // An hour of refusing bytes with nobody attached, which must not count.
        for second in 0..3_600 {
            assert!(!watch.note_tx(false, true, second * 1_000));
        }
        let arrived = 3_600_000;
        assert!(!wedged(&mut watch, arrived), "reset the instant a host said hello");
        assert!(!wedged(&mut watch, arrived + TX_STALL_TIMEOUT_MS - 1));
        assert!(wedged(&mut watch, arrived + TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn a_host_gone_longer_than_the_window_stops_counting_and_starts_over() {
        let mut watch = StallWatch::new();
        assert!(!wedged(&mut watch, 0));
        // The bench case, ticked at the millisecond the loop really runs at:
        // the endpoint refuses throughout while the host says nothing for
        // longer than the window. Presence lapses part-way through, so the
        // stall clock is dropped rather than carried across the gap, and the
        // host's return starts it from nothing.
        let back = HOST_PRESENT_WINDOW_MS + 1_000;
        for ms in 1..back {
            assert!(!watch.note_tx(false, true, ms), "reset {ms} ms into the silence");
        }
        assert!(!wedged(&mut watch, back));
        assert!(!wedged(&mut watch, back + TX_STALL_TIMEOUT_MS - 1));
        assert!(wedged(&mut watch, back + TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn a_host_seen_once_long_ago_is_not_a_host() {
        let mut watch = StallWatch::new();
        watch.note_host(0);
        for ms in HOST_PRESENT_WINDOW_MS..HOST_PRESENT_WINDOW_MS + 60_000 {
            assert!(!watch.note_tx(false, true, ms));
        }
    }
}
