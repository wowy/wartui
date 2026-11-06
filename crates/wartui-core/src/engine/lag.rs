use wartui_proto::bus::Slot;

use super::{FleetEngine, Now};

/// A lag large enough that [`FleetEngine::reading_live`] says no: 100 ms, past which a heartbeat
/// says what a node held then rather than now.
pub(crate) const BEHIND_US: u64 = 100_000;

/// A change in whether the host is measurably behind the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagTransition {
    FellBehind,
    CaughtUp,
}

/// What a lag measured from two arrivals changes, given whether a spell behind the bridge is open.
/// Only a measured lag opens a spell, so a session's first frame logs nothing.
fn lag_transition(behind: bool, lag_us: u64) -> Option<LagTransition> {
    match (behind, lag_us >= BEHIND_US) {
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
        // The host out-waited the bridge: nothing is queued.
        0
    } else {
        current_lag_us.saturating_add(lag_increase_us)
    }
}

impl FleetEngine {
    /// Track whether this host is reading the link in real time.
    ///
    /// A host that falls behind a busy fleet reads frames queued at the bridge, or in the pipe
    /// between, later than they were heard. Frames carry no age, but the bridge stamps each with
    /// its own clock, which ticks at the host's rate. Read live, the stamps keep pace with the
    /// host. Draining a queue, they run ahead. The gap accumulates into [`Self::backlog_lag_us`],
    /// and resets once the host waits longer for a frame than the bridge took to produce it, which
    /// happens only with nothing queued.
    ///
    /// A connection starts caught up, and its first frame counts as live. Nothing older can reach
    /// it: the bridge queues nothing for an absent host and clears its queue when one connects, and
    /// the transport drops whatever decodes ahead of the connection's first `Ready`
    /// (`wartui_bridge::serial`, `read_loop`), which is what was already past that queue.
    ///
    /// This is an estimate, not clock synchronization. Its jobs:
    ///
    /// - The "host fell behind the bridge" and "caught up" logs, and [`Self::lag_peak_us`] for
    ///   the host status row.
    /// - A heartbeat read behind is history: its epoch does not set what the node holds, and it
    ///   neither reads as a restart nor flags an assignment as unadopted (`on_heartbeat`).
    /// - `note_batch_seq`'s replay flag: a gap after a replayed batch is the host link's loss,
    ///   not the node's.
    /// - The `live` flag each stored heartbeat carries.
    pub(super) fn note_arrival(&mut self, slot: Slot, rx_us: u32, now: Now) {
        // A first frame has nothing to be timed against, and the connection started caught up.
        if let Some((last_rx_us, last_mono)) = self.last_arrival {
            let bridge_elapsed_us = u64::from(rx_us.wrapping_sub(last_rx_us));
            let host_elapsed_us = now.mono.saturating_duration_since(last_mono).as_micros() as u64;

            self.backlog_lag_us =
                calculate_updated_lag(self.backlog_lag_us, bridge_elapsed_us, host_elapsed_us);
            self.lag_peak_us = self.lag_peak_us.max(self.backlog_lag_us);
            let lag_us = self.backlog_lag_us;
            match lag_transition(self.behind_since.is_some(), lag_us) {
                Some(LagTransition::FellBehind) => {
                    self.behind_since = Some(now.mono);
                    self.behind_peak_us = lag_us;
                    tracing::info!(
                        %slot,
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
        self.last_arrival = Some((rx_us, now.mono));
    }

    /// End the spell behind the bridge that [`Self::note_arrival`] opened, and say so.
    fn close_behind(&mut self, now: Now) {
        let Some(since) = self.behind_since.take() else { return };
        let peak_us = std::mem::take(&mut self.behind_peak_us);
        let lasted_ms = now.mono.saturating_duration_since(since).as_millis() as u64;
        tracing::info!(peak_us, lasted_ms, "host caught up with the bridge");
    }

    /// Forget every arrival and assume the host is caught up, as on start: the link just came up
    /// or went down, and a connection starts with nothing stale in it ([`Self::note_arrival`]). An
    /// open spell ends unlogged, since no frame said the host caught up.
    pub(super) fn reset_arrivals(&mut self) {
        self.last_arrival = None;
        self.backlog_lag_us = 0;
        self.behind_since = None;
        self.behind_peak_us = 0;
    }

    /// Whether the host is reading the link live, so a record handled now says what holds now. A
    /// stale heartbeat still proves the node, but the epoch it reports is history.
    pub(super) fn reading_live(&self) -> bool {
        self.backlog_lag_us < BEHIND_US
    }
}

#[cfg(test)]
mod tests {
    use super::{BEHIND_US, LagTransition, lag_transition};

    #[test]
    fn lag_transition_falls_behind_when_measured_lag_reaches_threshold() {
        assert_eq!(lag_transition(false, BEHIND_US), Some(LagTransition::FellBehind));
        assert_eq!(lag_transition(false, BEHIND_US - 1), None);
    }

    #[test]
    fn lag_transition_catches_up_when_lag_drops_below_threshold() {
        assert_eq!(lag_transition(true, 0), Some(LagTransition::CaughtUp));
        assert_eq!(lag_transition(true, BEHIND_US - 1), Some(LagTransition::CaughtUp));
    }

    #[test]
    fn lag_transition_reports_nothing_when_spell_continues() {
        // One line per transition, never per frame.
        assert_eq!(lag_transition(true, BEHIND_US * 30), None);
        assert_eq!(lag_transition(false, 0), None);
    }
}
