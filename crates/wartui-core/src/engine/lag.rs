use std::time::Duration;

use wartui_proto::mac::{self, Mac};
use wartui_proto::plan;

use super::{FleetEngine, Now};

/// A lag large enough that [`FleetEngine::air_is_live`] says no, used as the
/// starting assumption on a connection whose backlog has not been seen yet.
pub(crate) const BEHIND_THE_AIR_US: u64 = plan::ADMIN_WAIT_MS as u64 * 1_000;

/// How long a link has to have been up with no frame at all before its first frame
/// is taken as live.
///
/// A bridge's backlog reaches the host within milliseconds of the port opening:
/// twenty-five frames spanning 8.5 minutes of bridge time arrived inside 17 ms of host
/// time (`docs/phase-4-findings.md`). A link silent for longer than that has nothing
/// queued, so the first frame it carries is the present. One second is generous.
pub(crate) const QUIET_CONNECT: Duration = Duration::from_secs(1);

/// A change in whether the host is measurably behind the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagTransition {
    FellBehind,
    CaughtUp,
}

/// What a lag measured from two arrivals changes, given whether a spell behind the
/// air is already open.
///
/// Only a measured lag opens a spell. The lag assumed on connect never does, so the
/// first frame of a session logs nothing; the second frame of a backlog measures
/// the lag and opens one.
fn lag_transition(behind: bool, lag_us: u64) -> Option<LagTransition> {
    match (behind, lag_us >= BEHIND_THE_AIR_US) {
        (false, true) => Some(LagTransition::FellBehind),
        (true, false) => Some(LagTransition::CaughtUp),
        _ => None,
    }
}

/// Calculates the updated backlog lag based on elapsed bridge and host time.
///
/// Returns `0` if the host elapsed time matches or exceeds the bridge elapsed time,
/// indicating that the queue has drained. Otherwise, returns the accumulated lag.
#[inline]
fn calculate_updated_lag(current_lag_us: u64, bridge_elapsed_us: u64, host_elapsed_us: u64) -> u64 {
    let lag_increase_us = bridge_elapsed_us.saturating_sub(host_elapsed_us);
    if lag_increase_us == 0 {
        // The host out-waited the air: nothing is queued behind this.
        0
    } else {
        current_lag_us.saturating_add(lag_increase_us)
    }
}

impl FleetEngine {
    /// Track whether this host is reading the link in real time.
    ///
    /// The bridge buffers what it hears while nothing is attached, so a fresh
    /// connection receives a ring's worth of the recent past as fast as USB will
    /// carry it — minutes of bridge time inside milliseconds of host time
    /// (`docs/phase-4-findings.md`).
    ///
    /// Nothing in a frame says how old it is, but the bridge stamps every one
    /// with its own clock and the two clocks tick at the same rate: read live the
    /// stamps advance in step with the host's, and while a backlog drains they
    /// run far ahead. The gap accumulates into [`Self::backlog_lag_us`] and
    /// resets the moment the host waits longer for a frame than the bridge spent
    /// producing one, which can only happen with nothing queued.
    ///
    /// A first frame has no earlier one to be timed against. It is taken as live
    /// only when the link has been up for [`QUIET_CONNECT`] without a frame, since
    /// a backlog would have arrived by then; otherwise the assumption that the host
    /// is behind stands.
    ///
    /// Deliberately an estimate rather than a clock synchronization. It has one
    /// job: to keep [`Self::send_admin`] from mistaking the past for the present.
    pub(super) fn note_arrival(&mut self, src: Mac, rx_us: u32, now: Now) {
        match self.last_arrival {
            Some((last_rx_us, last_mono)) => {
                let bridge_elapsed_us = u64::from(rx_us.wrapping_sub(last_rx_us));
                let host_elapsed_us =
                    now.mono.saturating_duration_since(last_mono).as_micros() as u64;

                self.backlog_lag_us =
                    calculate_updated_lag(self.backlog_lag_us, bridge_elapsed_us, host_elapsed_us);
                self.lag_peak_us = self.lag_peak_us.max(self.backlog_lag_us);
                let lag_us = self.backlog_lag_us;
                match lag_transition(self.behind_since.is_some(), lag_us) {
                    Some(LagTransition::FellBehind) => {
                        self.behind_since = Some(now.mono);
                        self.behind_peak_us = lag_us;
                        tracing::info!(
                            src = %mac::full(&src),
                            last_rx_us,
                            rx_us,
                            bridge_elapsed_us,
                            host_elapsed_us,
                            lag_us,
                            "host fell behind the bridge"
                        );
                    }
                    Some(LagTransition::CaughtUp) => self.close_behind(now),
                    None => self.behind_peak_us = self.behind_peak_us.max(lag_us),
                }
            }
            None => {
                let quiet = self
                    .link_up_since
                    .is_some_and(|since| now.mono.duration_since(since) >= QUIET_CONNECT);
                if quiet {
                    // Not a measurement, so not counted into `lag_peak_us`. No spell
                    // is open either: `reset_arrivals` closed it on the connect.
                    self.backlog_lag_us = 0;
                }
            }
        }
        self.last_arrival = Some((rx_us, now.mono));
    }

    /// End the spell behind the air that [`Self::note_arrival`] opened, and say so.
    fn close_behind(&mut self, now: Now) {
        let Some(since) = self.behind_since.take() else { return };
        let peak_us = std::mem::take(&mut self.behind_peak_us);
        let lasted_ms = now.mono.saturating_duration_since(since).as_millis() as u64;
        tracing::info!(peak_us, lasted_ms, "host caught up with the bridge");
    }

    /// Forget every arrival and assume the host is behind, as on start: the link has
    /// just come up or gone down, and a backlog may be on its way. A spell behind the
    /// air ends here unlogged, since no frame said the host caught up.
    pub(super) fn reset_arrivals(&mut self) {
        self.last_arrival = None;
        self.backlog_lag_us = BEHIND_THE_AIR_US;
        self.behind_since = None;
        self.behind_peak_us = 0;
    }

    /// Whether a frame being handled now is recent enough to act on.
    ///
    /// Only assignments care. A stale heartbeat is still a heartbeat — the node
    /// was alive and its radio is what it said — but the 100 ms window it opened
    /// shut long ago, so transmitting into it reaches nothing.
    pub(super) fn air_is_live(&self) -> bool {
        self.backlog_lag_us < BEHIND_THE_AIR_US
    }
}

#[cfg(test)]
mod tests {
    use super::{BEHIND_THE_AIR_US, LagTransition, lag_transition};

    #[test]
    fn lag_transition_falls_behind_when_measured_lag_reaches_threshold() {
        assert_eq!(lag_transition(false, BEHIND_THE_AIR_US), Some(LagTransition::FellBehind));
        assert_eq!(lag_transition(false, BEHIND_THE_AIR_US - 1), None);
    }

    #[test]
    fn lag_transition_catches_up_when_lag_drops_below_threshold() {
        assert_eq!(lag_transition(true, 0), Some(LagTransition::CaughtUp));
        assert_eq!(lag_transition(true, BEHIND_THE_AIR_US - 1), Some(LagTransition::CaughtUp));
    }

    #[test]
    fn lag_transition_reports_nothing_when_spell_continues() {
        // One line per transition, never per frame.
        assert_eq!(lag_transition(true, BEHIND_THE_AIR_US * 30), None);
        assert_eq!(lag_transition(false, 0), None);
    }
}
