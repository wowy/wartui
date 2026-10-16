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
//!
//! The budget is checked between polls, so it bounds the polling, not the pass. A host
//! frame a poll completes is acted on inside the burst and adds its own time: a
//! `SendEspNow` waits about 3 ms for its send-complete callback.

/// The longest a burst waits for the next byte to move before giving the pass back.
///
/// Longer than the gap between packets from a host still writing a frame, or still
/// reading one: a full-speed bulk packet is about 50 µs on the bus. Beyond it the host has
/// stopped, and spinning on is a core burned for nothing.
pub const BURST_GAP_US: u64 = 200;

/// The longest a burst goes on polling, however much is moving.
///
/// About one 1 KB frame at USB speed. The bound is the radio: `esp-radio`'s receive queue
/// is ten frames deep and drops its oldest, and the burst does not drain it. Polling this
/// long lets at most a few frames arrive against it. A host frame the burst completes adds
/// the time it takes to act on.
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
