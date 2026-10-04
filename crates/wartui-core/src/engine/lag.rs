use std::time::Duration;

use wartui_proto::mac::{self, Mac};
use wartui_proto::plan;

use super::{FleetEngine, Now};

/// A lag large enough that [`FleetEngine::air_is_live`] says no: the assumption on a connection
/// whose backlog has not been seen yet.
pub(crate) const BEHIND_THE_AIR_US: u64 = plan::ADMIN_WAIT_MS as u64 * 1_000;

/// How long a link must be up with no frame before its first frame counts as live.
///
/// A backlog arrives within milliseconds of the port opening: twenty-five frames spanning
/// 8.5 minutes of bridge time arrived inside 17 ms of host time (`docs/phase-4-findings.md`). A
/// link quiet for longer has nothing queued. One second is generous.
pub(crate) const QUIET_CONNECT: Duration = Duration::from_secs(1);

/// A change in whether the host is measurably behind the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagTransition {
    FellBehind,
    CaughtUp,
}

/// What a lag measured from two arrivals changes, given whether a spell behind the air is open.
/// Only a measured lag opens a spell, never the one assumed on connect. So a session's first frame
/// logs nothing, and a backlog's second frame opens the spell.
fn lag_transition(behind: bool, lag_us: u64) -> Option<LagTransition> {
    match (behind, lag_us >= BEHIND_THE_AIR_US) {
        (false, true) => Some(LagTransition::FellBehind),
        (true, false) => Some(LagTransition::CaughtUp),
        _ => None,
    }
}

/// The backlog lag after one more arrival. Zero once the host waited at least as long as the bridge
/// took, since the queue has drained. Otherwise the difference adds up.
#[inline]
fn calculate_updated_lag(current_lag_us: u64, bridge_elapsed_us: u64, host_elapsed_us: u64) -> u64 {
    let lag_increase_us = bridge_elapsed_us.saturating_sub(host_elapsed_us);
    if lag_increase_us == 0 {
        // The host out-waited the air: nothing is queued.
        0
    } else {
        current_lag_us.saturating_add(lag_increase_us)
    }
}

impl FleetEngine {
    /// Track whether this host is reading the link in real time.
    ///
    /// With nothing attached, the bridge buffers what it hears. A new connection then receives a
    /// ring's worth of the past as fast as USB carries it: minutes of bridge time in milliseconds
    /// of host time (`docs/phase-4-findings.md`).
    ///
    /// Frames carry no age, but the bridge stamps each with its own clock, which ticks at the
    /// host's rate. Read live, the stamps keep pace with the host. Draining a backlog, they run far
    /// ahead. The gap accumulates into [`Self::backlog_lag_us`], and resets once the host waits
    /// longer for a frame than the bridge took to produce it, which happens only with nothing
    /// queued.
    ///
    /// A first frame has nothing to be timed against. It counts as live only after
    /// [`QUIET_CONNECT`] of silence, by which time any backlog would have arrived. This is an
    /// estimate, not clock synchronization, with one job: keeping [`Self::send_admin`] from
    /// mistaking the past for the present.
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
                    // Not a measurement, so not in `lag_peak_us`. `reset_arrivals` already closed
                    // any spell.
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

    /// Forget every arrival and assume the host is behind, as on start: the link just came up or
    /// went down, and a backlog may follow. An open spell ends unlogged, since no frame said the
    /// host caught up.
    pub(super) fn reset_arrivals(&mut self) {
        self.last_arrival = None;
        self.backlog_lag_us = BEHIND_THE_AIR_US;
        self.behind_since = None;
        self.behind_peak_us = 0;
    }

    /// Whether a frame being handled now is recent enough to act on. Only assignments care: a stale
    /// heartbeat still proves the node and its radio, but its 100 ms window shut long ago.
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
