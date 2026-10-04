//! Deciding when the bridge's USB transmit endpoint has stopped draining.
//!
//! Only the firmware acts on this, and it lives here for the reason
//! [`crate::outbox`] does: a `no_std` binary cannot run a test. The rule below
//! has shipped three defects, none found by review — the first took a bridge on a
//! bench beside a talking fleet, the second an operator quitting a session and
//! watching the board reset three seconds later, the third a fault build that a
//! host asking once could never clear. Each is one line of arithmetic against a
//! clock, and each is now a test a few microseconds long.
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
/// (`crates/wartui-core/src/engine/config.rs`), because a connected host with a quiet
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
/// - something is queued and this pass moved none of it, so silence is not
///   simply having nothing to say;
/// - a host frame *that asks for a reply* has decoded since the last byte moved,
///   so somebody has asked for something the endpoint has not delivered;
/// - a host frame decoded inside [`HOST_PRESENT_WINDOW_MS`], so that host is
///   still there;
/// - and [`TX_STALL_TIMEOUT_MS`] has passed since *both* the first such frame and
///   the first refusal, so the endpoint is not merely slow.
///
/// One unanswered asking frame is enough. The host sends a single `Identify` per
/// connection (`crates/wartui-bridge/src/serial.rs`, `connect`), so a bridge that
/// wedged before it connected reboots three seconds after that frame decodes.
///
/// # Why the clock starts at the contradiction
///
/// Timing from the last byte written looks equivalent and is not. A bridge left
/// powered beside a talkative fleet fills its rings with nobody reading, so by the
/// time an operator attaches the last byte moved *hours* ago — and all of it would
/// count against a transmit path with nothing wrong with it. So the clock runs
/// from the later of the first refusal and the first asking host frame since the
/// last byte moved: neither half alone is a contradiction. The *first* such frame, not
/// the latest, because a host polling twice a second through a wedge would
/// otherwise push the clock forward for ever.
///
/// # Why a frame must be unanswered, not merely recent
///
/// A host that has *quit* satisfies "spoke inside the window" for a further
/// [`HOST_PRESENT_WINDOW_MS`], and quitting is exactly what stops the endpoint
/// draining — so with a fleet in earshot the rings fill the moment it lets go, and
/// since [`TX_STALL_TIMEOUT_MS`] is the shorter of the two a presence test alone
/// resets the board after every session. A host that was reading moved bytes after
/// its last frame, which clears the unanswered frame, so the stall that follows its
/// quit has no frame to time against. A slow host that lets a byte through just
/// inside every timeout clears it the same way, and is never reset.
///
/// A frame that asks for nothing cannot be left unanswered, so it never starts the
/// clock: it proves presence and no more. That matters because a panel push, sent
/// up to once a second to a bridge with a screen, is often the last thing a TUI sends
/// before it quits, and no byte follows it. Counting it would reset the board a few
/// seconds after an ordinary quit, as soon as a node frame was queued behind the dead
/// endpoint. A quiet `AddPeer` or `RemovePeer` would do the same.
///
/// The cost is a host that sends a frame that asks and leaves before any byte moves
/// after it — killed mid-handshake, say. If something is queued, that bridge reboots
/// three seconds later, and only while the frame is inside
/// [`HOST_PRESENT_WINDOW_MS`]. The next connection reads `TxStalled` in its
/// `Ready`, and nothing else is lost.
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
    /// When the transmit path first refused a byte with something queued, or
    /// `None` if it is not refusing them now.
    stall_since: Option<u64>,
    /// When the first host frame that asks for a reply decoded since the last byte
    /// moved, or `None` if none has.
    host_since_move: Option<u64>,
    /// When a frame from the host last decoded, or `None` if none ever has.
    last_host: Option<u64>,
}

impl StallWatch {
    /// Nothing queued, nobody here, no stall in progress.
    #[must_use]
    pub const fn new() -> Self {
        Self { stall_since: None, host_since_move: None, last_host: None }
    }

    /// Record proof of a host: a frame that decoded, at `now_ms`.
    ///
    /// Only a frame that *decoded* may be reported: a board running node firmware
    /// talks constantly down the same wire and none of it is a frame. `asks` is
    /// [`HostToBridge::asks_for_reply`](crate::link::HostToBridge::asks_for_reply):
    /// every frame proves presence, but only one that asks can start the clock.
    pub const fn note_host(&mut self, now_ms: u64, asks: bool) {
        self.last_host = Some(now_ms);
        if asks && self.host_since_move.is_none() {
            self.host_since_move = Some(now_ms);
        }
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
    /// anything is still waiting to go. `true` means the four facts above all
    /// hold and the only remedy left is a reset.
    ///
    /// `now_ms` must never go backwards. The firmware reads it from the boot
    /// instant, which cannot, and a caller that breaks the rule is caught by the
    /// subtraction below rather than mistaken for a host that is present.
    pub fn note_tx(&mut self, moved: bool, queued: bool, now_ms: u64) -> bool {
        if moved {
            // Delivered: every frame so far has been answered as far as this end
            // can tell, and the endpoint is draining.
            self.stall_since = None;
            self.host_since_move = None;
            return false;
        }
        if !queued {
            // Nothing refused, but nothing answered either: a frame that asked
            // for something still counts once there is something to send.
            self.stall_since = None;
            return false;
        }
        let refusing = *self.stall_since.get_or_insert(now_ms);
        let Some(asked) = self.host_since_move else { return false };
        let host_here = self.last_host.is_some_and(|at| now_ms - at < HOST_PRESENT_WINDOW_MS);
        host_here && now_ms - asked.max(refusing) >= TX_STALL_TIMEOUT_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pass of the loop: the host spoke, then the pump moved nothing
    /// against a queue that is not empty. The wedge, in other words.
    fn wedged(watch: &mut StallWatch, now_ms: u64) -> bool {
        watch.note_host(now_ms, true);
        watch.note_tx(false, true, now_ms)
    }

    #[test]
    fn stall_watch_remains_clear_when_host_has_never_communicated() {
        let mut watch = StallWatch::new();
        // A bench bridge beside a talking fleet: rings full, endpoint refusing,
        // for an hour.
        for second in 0..3_600 {
            assert!(!watch.note_tx(false, true, second * 1_000));
        }
    }

    #[test]
    fn stall_watch_trips_reboot_when_tx_stalls_while_host_communicates() {
        let mut watch = StallWatch::new();
        assert!(!wedged(&mut watch, 0));
        assert!(!wedged(&mut watch, TX_STALL_TIMEOUT_MS - 1));
        assert!(wedged(&mut watch, TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn stall_watch_remains_clear_when_host_disconnects_after_sending_frame() {
        let mut watch = StallWatch::new();
        // The realistic quit: the host's last frame is answered, it closes the
        // port, and the fleet fills the rings behind it. Presence goes on
        // believing in it for the whole window, and the answered frame is what
        // stands in the way.
        watch.note_host(0, true);
        assert!(!watch.note_tx(true, true, 0));
        for ms in 1..60_000 {
            assert!(!watch.note_tx(false, true, ms), "reset {ms} ms after the host quit");
        }
    }

    #[test]
    fn stall_watch_resets_timer_when_tx_makes_forward_progress() {
        let mut watch = StallWatch::new();
        // A congested endpoint that keeps almost catching up: it refuses for
        // just under the timeout, lets one byte through, and does it again.
        // That is a slow host, not a wedged one, and it may go on for ever.
        for round in 0..100 {
            let base = round * TX_STALL_TIMEOUT_MS;
            assert!(!wedged(&mut watch, base));
            assert!(!wedged(&mut watch, base + TX_STALL_TIMEOUT_MS - 1));
            watch.note_host(base + TX_STALL_TIMEOUT_MS, true);
            assert!(!watch.note_tx(true, true, base + TX_STALL_TIMEOUT_MS));
        }
    }

    #[test]
    fn stall_watch_remains_clear_when_outbox_is_empty() {
        let mut watch = StallWatch::new();
        // A quiet fleet and an attentive host: nothing to say, and saying it
        // for an hour is not evidence of anything.
        for second in 0..3_600 {
            watch.note_host(second * 1_000, true);
            assert!(!watch.note_tx(false, false, second * 1_000));
        }
    }

    #[test]
    fn stall_watch_starts_timer_only_when_host_arrives_at_full_outbox() {
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
    fn stall_watch_resets_stall_clock_when_host_exceeds_presence_window() {
        let mut watch = StallWatch::new();
        // The host's frame at 0 is answered, then it says nothing for longer
        // than the window while the endpoint refuses throughout, ticked at the
        // millisecond the loop really runs at.
        watch.note_host(0, true);
        assert!(!watch.note_tx(true, true, 0));
        let back = HOST_PRESENT_WINDOW_MS + 1_000;
        for ms in 1..back {
            assert!(!watch.note_tx(false, true, ms), "reset {ms} ms into the silence");
        }
        // Its return is a frame nothing answers, and the clock runs from that
        // frame rather than from the refusal that began a window ago.
        watch.note_host(back, true);
        for ms in back..back + TX_STALL_TIMEOUT_MS {
            assert!(!watch.note_tx(false, true, ms), "reset {} ms after the return", ms - back);
        }
        assert!(watch.note_tx(false, true, back + TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn stall_watch_trips_reboot_when_host_asks_once_at_real_cadence() {
        // What the host sends any board: one `Identify` as the port opens, then
        // waiting, with the bridge's loop passing every 100 ms against an
        // endpoint that refuses.
        let mut watch = StallWatch::new();
        watch.note_host(0, true);
        assert!(!watch.note_tx(false, true, 0));
        for ms in (100..TX_STALL_TIMEOUT_MS).step_by(100) {
            assert!(!watch.note_tx(false, true, ms), "reset {ms} ms after one ask");
        }
        assert!(!watch.note_tx(false, true, TX_STALL_TIMEOUT_MS - 1));
        assert!(watch.note_tx(false, true, TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn stall_watch_times_first_unanswered_frame_when_host_polls_during_wedge() {
        // A capture polling twice a second through a wedge must not keep moving
        // the clock: it runs from the first frame nothing answered.
        let mut watch = StallWatch::new();
        for ms in (0..TX_STALL_TIMEOUT_MS).step_by(500) {
            assert!(!wedged(&mut watch, ms), "reset {ms} ms into the polling");
        }
        assert!(watch.note_tx(false, true, TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn stall_watch_times_refusal_when_outbox_fills_after_host_frame() {
        // The host asks while there is nothing to send, and the outbox stays
        // empty for two seconds. Nothing was refused in that time, so the clock
        // runs from the first refusal and not from the frame.
        let mut watch = StallWatch::new();
        watch.note_host(0, true);
        for ms in 0..2_000 {
            assert!(!watch.note_tx(false, false, ms));
        }
        let refused = 2_000;
        for ms in refused..refused + TX_STALL_TIMEOUT_MS {
            assert!(!watch.note_tx(false, true, ms), "reset {} ms into the stall", ms - refused);
        }
        assert!(watch.note_tx(false, true, refused + TX_STALL_TIMEOUT_MS));
    }

    #[test]
    fn stall_watch_remains_clear_when_host_exits_before_anything_moves_and_window_lapses() {
        // A host killed mid-handshake: one frame, nothing ever moved, and the
        // endpoint refusing from the window's end onwards. The frame is still
        // unanswered, but nobody is there to be answered.
        let mut watch = StallWatch::new();
        watch.note_host(0, true);
        for ms in HOST_PRESENT_WINDOW_MS..HOST_PRESENT_WINDOW_MS + 60_000 {
            assert!(!watch.note_tx(false, true, ms));
        }
    }

    #[test]
    fn stall_watch_ignores_stale_host_when_presence_window_has_elapsed() {
        let mut watch = StallWatch::new();
        watch.note_host(0, true);
        assert!(!watch.note_tx(true, false, 0));
        for ms in HOST_PRESENT_WINDOW_MS..HOST_PRESENT_WINDOW_MS + 60_000 {
            assert!(!watch.note_tx(false, true, ms));
        }
    }

    #[test]
    fn stall_watch_remains_clear_when_host_quits_after_frame_without_reply() {
        // A panel push is the last thing a quitting TUI sends and nothing answers
        // it, so no byte ever moves after it. It proves presence and starts no clock.
        let mut watch = StallWatch::new();
        watch.note_host(0, false);
        for ms in 0..1_000 {
            assert!(!watch.note_tx(false, false, ms));
        }
        for ms in 1_000..60_000 {
            assert!(!watch.note_tx(false, true, ms), "reset {ms} ms after the host quit");
        }
    }

    #[test]
    fn stall_watch_trips_reboot_when_asking_frame_follows_silent_one() {
        let mut watch = StallWatch::new();
        watch.note_host(0, false);
        for ms in 0..2_000 {
            assert!(!watch.note_tx(false, true, ms));
        }
        watch.note_host(2_000, true);
        for ms in 2_000..5_000 {
            assert!(
                !watch.note_tx(false, true, ms),
                "reset {ms} ms in, before the asking frame aged"
            );
        }
        assert!(watch.note_tx(false, true, 5_000));
    }
}
