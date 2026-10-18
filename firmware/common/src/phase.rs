//! Where a bridge's loop was when it last stopped making progress, and whether a host was
//! present then, kept across a reset for the next life to report.
//!
//! In RTC fast memory and marked persistent, so it survives the resets that matter — the
//! panic handler's, the watchdog's, and the one [`StallWatch`] asks for — and is read back by
//! the next life. A power-on leaves it undefined, which is why [`boot_phase`] only believes
//! it when the reset reason says the RTC domain was not reset.
//!
//! `AtomicU8`s because a `static mut` cannot be written without `unsafe`, which is denied
//! here, and `portable-atomic`'s because those are the ones `esp-hal` marks persistable.
//! Never contended: only a bridge's main task touches them.
//!
//! [`StallWatch`]: wartui_proto::stall::StallWatch

use portable_atomic::{AtomicU8, AtomicU32, Ordering};
use wartui_proto::link::LoopPhase;
use wartui_proto::reset::ResetCause;

/// Where the loop was when it last stopped making progress.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static PHASE: AtomicU8 = AtomicU8::new(0);

/// Says that [`PHASE`] was written by a build that writes it.
///
/// Persistent RTC memory holds whatever the last thing to use it left there — the
/// previous firmware's data after a reflash, nothing in particular after a
/// power-on — and either would be reported as a phase with a straight face. A word
/// this build alone writes is what separates a marker from a coincidence.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static PHASE_VALID: AtomicU32 = AtomicU32::new(0);

/// Arbitrary, and only has to be unlikely. Reads as `war1` in a memory dump.
const PHASE_MAGIC: u32 = 0x7761_7231;

/// Whether a host frame had decoded within
/// [`HOST_PRESENT_WINDOW_MS`](wartui_proto::stall::HOST_PRESENT_WINDOW_MS) at the last
/// pass of the loop, kept for the next life to decide whether it speaks first
/// ([`ResetCause::speaks_first`]).
///
/// Persistent RTC memory for the reason [`PHASE`] is, and only believed under the same
/// conditions, see [`boot_speaks_first`].
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static HOST_PRESENT: AtomicU8 = AtomicU8::new(0);

/// Record where the loop is, for the next life to report.
pub fn mark(phase: LoopPhase) {
    PHASE.store(phase as u8, Ordering::Relaxed);
}

/// Claim the marker for this build.
///
/// Called only after [`boot_phase`] and [`boot_speaks_first`] have read the previous
/// life's, so a reset between the two cannot make the next boot trust a phase nobody
/// wrote.
pub fn claim() {
    PHASE_VALID.store(PHASE_MAGIC, Ordering::Relaxed);
}

/// Record, every pass, whether a host is present, for the next life.
pub fn note_host_present(present: bool) {
    HOST_PRESENT.store(u8::from(present), Ordering::Relaxed);
}

/// Whether the previous life ended with a host present, and so whether this one
/// speaks first.
///
/// Read before [`claim`], and trusted on the same terms as [`boot_phase`]. The flag is
/// then cleared, so a life that never sees a host records "absent".
#[must_use]
pub fn boot_speaks_first(cause: ResetCause) -> bool {
    let valid =
        !matches!(cause, ResetCause::PowerOn) && PHASE_VALID.load(Ordering::Relaxed) == PHASE_MAGIC;
    let was_present = valid && HOST_PRESENT.load(Ordering::Relaxed) == 1;
    HOST_PRESENT.store(0, Ordering::Relaxed);
    cause.speaks_first(was_present)
}

/// What the marker said, if this reset is one that preserved it.
///
/// A power-on gives uninitialized RTC memory; anything else reached us through a
/// reset that left the RTC domain alone, so the marker is real.
#[must_use]
pub fn boot_phase(cause: ResetCause) -> LoopPhase {
    if matches!(cause, ResetCause::PowerOn) || PHASE_VALID.load(Ordering::Relaxed) != PHASE_MAGIC {
        return LoopPhase::Unknown;
    }
    match PHASE.load(Ordering::Relaxed) {
        1 => LoopPhase::Boot,
        2 => LoopPhase::DrainRadio,
        3 => LoopPhase::DrainLink,
        4 => LoopPhase::Command,
        5 => LoopPhase::Transmit,
        6 => LoopPhase::Pump,
        7 => LoopPhase::Idle,
        8 => LoopPhase::TxStalled,
        9 => LoopPhase::Render,
        10 => LoopPhase::SpiTransfer,
        _ => LoopPhase::Unknown,
    }
}
