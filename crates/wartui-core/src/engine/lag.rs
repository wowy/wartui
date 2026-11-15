use wartui_proto::bus::Slot;

use super::{FleetEngine, Now};
use crate::record::HostLink;

/// The lag at which a host reading over USB, or a simulated link, is behind: 100 ms. Over USB the
/// lag stays under a millisecond, so a lag near this is a backlog draining, and a heartbeat read
/// past it says what a node held then rather than now.
pub const BEHIND_USB_US: u64 = 100_000;

/// The lag at which a host reading over Bluetooth is behind: 500 ms. A Bluetooth link delivers
/// once per connection interval, so frames arrive in clumps and the estimate reads delivery
/// jitter. One drive at a 48.75 ms interval measured that jitter at up to 203 ms with no backlog.
/// A real backlog grows without bound, so it crosses either line.
pub const BEHIND_BLUETOOTH_US: u64 = 500_000;

/// The lag at which a host reading over `link` is behind the bridge, so that
/// [`FleetEngine::reading_live`] says no. `None` is a simulated link.
#[must_use]
pub const fn behind_us(link: Option<HostLink>) -> u64 {
    match link {
        Some(HostLink::Bluetooth) => BEHIND_BLUETOOTH_US,
        Some(HostLink::Usb) | None => BEHIND_USB_US,
    }
}

/// A change in whether the host is measurably behind the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagTransition {
    FellBehind,
    CaughtUp,
}

/// What a lag measured from two arrivals changes, given whether a spell behind the bridge is open.
/// Only a measured lag opens a spell, so a session's first frame logs nothing. `line_us` is the
/// link's threshold ([`behind_us`]).
fn lag_transition(behind: bool, lag_us: u64, line_us: u64) -> Option<LagTransition> {
    match (behind, lag_us >= line_us) {
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
    /// (`wartui_bridge`'s `gate`), which is what was already past that queue.
    ///
    /// This is an estimate, not clock synchronization. Its jobs:
    ///
    /// - The "host fell behind the bridge" and "caught up" logs, and [`Self::lag_peak_us`] for
    ///   the host status row.
    /// - A heartbeat read behind is history (`on_heartbeat`): its epoch does not set what the node
    ///   holds, so the view does not show it adopting from one, and an epoch of 0 in it does not
    ///   read as a restart. A falling counter in it still reads as a restart.
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
            match lag_transition(self.behind_since.is_some(), lag_us, behind_us(self.config.link)) {
                Some(LagTransition::FellBehind) => {
                    self.behind_since = Some(now.mono);
                    self.behind_peak_us = lag_us;
                    tracing::info!(
                        slot = %self.slot_label(slot),
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
        self.backlog_lag_us < behind_us(self.config.link)
    }
}

#[cfg(test)]
mod tests {
    use super::{BEHIND_BLUETOOTH_US, BEHIND_USB_US, LagTransition, behind_us, lag_transition};
    use std::time::{Duration, Instant};

    use wartui_proto::bus::Slot;

    use crate::engine::{EngineConfig, FleetEngine, Now};
    use crate::record::HostLink;

    const LINE: u64 = BEHIND_USB_US;

    /// An engine over `link` that has read five frames 50 ms apart on the bridge's clock, a
    /// millisecond apart on the host's: about 200 ms of lag.
    fn clumped(link: Option<HostLink>) -> FleetEngine {
        let base = Instant::now();
        let at = |ms: u64| Now { mono: base + Duration::from_millis(ms), unix_ms: 0 };
        let mut engine = FleetEngine::new(EngineConfig { link, ..EngineConfig::default() }, at(0));
        let slot = Slot::new(0, 0).expect("a slot");
        for n in 0..5u32 {
            engine.note_arrival(slot, n * 50_000, at(u64::from(n)));
        }
        engine
    }

    #[test]
    fn lag_transition_falls_behind_when_measured_lag_reaches_threshold() {
        assert_eq!(lag_transition(false, LINE, LINE), Some(LagTransition::FellBehind));
        assert_eq!(lag_transition(false, LINE - 1, LINE), None);
    }

    #[test]
    fn lag_transition_catches_up_when_lag_drops_below_threshold() {
        assert_eq!(lag_transition(true, 0, LINE), Some(LagTransition::CaughtUp));
        assert_eq!(lag_transition(true, LINE - 1, LINE), Some(LagTransition::CaughtUp));
    }

    #[test]
    fn lag_transition_reports_nothing_when_spell_continues() {
        // One line per transition, never per frame.
        assert_eq!(lag_transition(true, LINE * 30, LINE), None);
        assert_eq!(lag_transition(false, 0, LINE), None);
    }

    #[test]
    fn behind_us_gives_usb_threshold_when_link_is_usb_or_simulated() {
        assert_eq!(behind_us(Some(HostLink::Usb)), BEHIND_USB_US);
        assert_eq!(behind_us(None), BEHIND_USB_US);
    }

    #[test]
    fn behind_us_gives_wider_threshold_when_link_is_bluetooth() {
        assert_eq!(behind_us(Some(HostLink::Bluetooth)), BEHIND_BLUETOOTH_US);
        const { assert!(BEHIND_BLUETOOTH_US > BEHIND_USB_US) };
    }

    #[test]
    fn engine_opens_no_spell_when_bluetooth_clump_lags_under_its_threshold() {
        let engine = clumped(Some(HostLink::Bluetooth));
        assert!(engine.reading_live());
        assert!(engine.behind_since.is_none(), "no spell behind the bridge");
    }

    #[test]
    fn engine_opens_spell_when_usb_clump_lags_over_its_threshold() {
        let engine = clumped(Some(HostLink::Usb));
        assert!(!engine.reading_live());
        assert!(engine.behind_since.is_some(), "a spell behind the bridge");
    }
}
