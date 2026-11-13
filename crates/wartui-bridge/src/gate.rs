//! What one connection admits from the bridge's stream, whichever transport carries it.
//!
//! **A connection starts caught up.** A bridge clears its host queue when a host attaches and
//! queues `Ready` first, so whatever decodes ahead of a connection's first `Ready` was written
//! before the connection: out of a CH343's or a tty's buffer, or past the C6 before a central
//! subscribed. It is stale, and a heartbeat seconds old reads as a host behind the bridge
//! (`wartui_core::engine::lag`). So [`Gate`] drops it.
//!
//! **A `Ready` is either a new life or a second answer.** Two arrive on one connection for two
//! different reasons, and the uptime separates them ([`is_a_new_life`]). One is a second answer to
//! an `Identify` sent before the first came back: same life, and reporting it would look like a
//! reconnect that never happened. The other is a new life under a held connection — a C6 that
//! restarted, or a P4 that rebooted while the CH343 stayed on the bus — and that has to be
//! reported, or the engine keeps the dead life's `BridgeInfo`.

use std::time::{Duration, Instant};

use wartui_proto::link::BridgeToHost;
use wartui_proto::mac;

use crate::BridgeInfo;

/// What [`Gate::admit`] makes of one decoded frame.
// Large because a message carries an inline node record, as `LinkEvent::Message` does. Each is
// handed on at once, and boxing it would allocate for every frame.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admitted {
    /// A `Ready`. `first` is this connection's first; `fresh` is a new life, which is reported.
    Ready {
        /// The bridge as it announced itself.
        info: BridgeInfo,
        /// This connection's first `Ready`, which proves the board.
        first: bool,
        /// A life this connection has not reported yet.
        fresh: bool,
    },
    /// A frame to pass on.
    Message(BridgeToHost),
    /// A frame from before this connection's first `Ready`, dropped.
    Stale,
}

/// One connection's view of the stream: the last `Ready`, and the frames dropped before the
/// first.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    /// The last `Ready`'s uptime and the host's time it arrived. A property of one connection,
    /// not of the link.
    last_ready: Option<(u32, Instant)>,
    /// Frames that decoded before this connection's first `Ready`, and were not passed on.
    skipped: u32,
}

impl Gate {
    /// A connection that has heard nothing yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether this connection has had its first `Ready`.
    pub(crate) fn announced(&self) -> bool {
        self.last_ready.is_some()
    }

    /// Frames dropped before the first `Ready`.
    pub(crate) fn skipped(&self) -> u32 {
        self.skipped
    }

    /// Start over, as a new connection on the same link: the next `Ready` is reported whatever
    /// its uptime, and what decodes before it is dropped.
    pub(crate) fn restart(&mut self) {
        *self = Self::new();
    }

    /// Take one frame that decoded, arriving at `now`.
    pub(crate) fn admit(&mut self, msg: BridgeToHost, now: Instant) -> Admitted {
        match msg {
            BridgeToHost::Ready {
                mac,
                fw_version,
                reset_cause,
                last_phase,
                uptime_ms,
                node_board,
                radio,
                image,
            } => {
                let first = self.last_ready.is_none();
                let fresh = is_a_new_life(uptime_ms, now, self.last_ready);
                if first && self.skipped > 0 {
                    tracing::debug!("skipped {} frames from before this connection", self.skipped);
                }
                self.last_ready = Some((uptime_ms, now));
                if fresh {
                    // Without it, a log of a link that dropped and came back ends at "opening
                    // the bridge" — as does one that never did.
                    tracing::info!(
                        mac = %mac::full(&mac),
                        fw = %fw_version.as_str(),
                        reset = ?reset_cause,
                        phase = ?last_phase,
                        uptime_ms,
                        board = %node_board,
                        radio = ?radio,
                        "the bridge announced itself"
                    );
                }
                let info = BridgeInfo {
                    mac,
                    fw_version: fw_version.as_str().to_owned(),
                    reset_cause,
                    last_phase,
                    uptime_ms,
                    node_board,
                    radio,
                    image,
                };
                Admitted::Ready { info, first, fresh }
            }
            msg => {
                // A rebooted bridge's `Ready` on a connection already announced changes nothing
                // here.
                if self.last_ready.is_none() {
                    self.skipped += 1;
                    return Admitted::Stale;
                }
                log_bridge_line(&msg);
                Admitted::Message(msg)
            }
        }
    }
}

/// Puts a line the bridge sent in the log: a `Log` at info, an `Error` as a warning. Here rather
/// than in the engine, which does no I/O and drops these frames.
fn log_bridge_line(msg: &BridgeToHost) {
    match msg {
        BridgeToHost::Error { message } => tracing::warn!("bridge: {message}"),
        BridgeToHost::Log { message } => tracing::info!("bridge: {message}"),
        _ => {}
    }
}

/// How far a `Ready` may fall short of the last life's projected uptime and still be
/// that life.
///
/// It absorbs delivery delay: a second answer to one `Identify`, read late behind a
/// backlog, falls short by the time it waited. The bridge sends `Ready` ahead of its queue
/// and the host's measured lag is well under this.
pub(crate) const READY_SLACK: Duration = Duration::from_secs(2);

/// Whether a `Ready` arriving at `now` came from a life that started after the last one
/// seen, which `last_seen` gives as its uptime and the host's time it arrived.
///
/// The first is always a new life. After that, a `Ready` is a new life when either holds:
///
/// - **Its uptime is below the last `Ready`'s.** One life's clock never falls, so this
///   catches a restart however soon it follows, as when `wartui reset` reboots a bridge
///   a second after its first `Ready`.
/// - **Its uptime is below the last life's projection by more than [`READY_SLACK`]**, the
///   projection being the last uptime plus the time since it arrived. This catches a C6 that
///   restarts under a held connection seconds after a `Ready` carried the uptime of a C6
///   reset with the P4, where the new life's uptime is above that one.
///
/// What neither catches is a restart whose uptime is above the last `Ready`'s and within
/// the slack of its projection. The module's `//!` says why both lives arrive on one
/// connection.
pub(crate) fn is_a_new_life(
    uptime_ms: u32,
    now: Instant,
    last_seen: Option<(u32, Instant)>,
) -> bool {
    last_seen.is_none_or(|(previous, arrived)| {
        let projected = u128::from(previous) + now.saturating_duration_since(arrived).as_millis();
        uptime_ms < previous || u128::from(uptime_ms) + READY_SLACK.as_millis() < projected
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use wartui_proto::bus::{NodeBoard, Slot};
    use wartui_proto::link::{BridgeToHost, FromNodeBody, LoopPhase, ShortStr};
    use wartui_proto::reset::ResetCause;

    use super::{Admitted, Gate};

    fn ready(uptime_ms: u32) -> BridgeToHost {
        BridgeToHost::Ready {
            mac: [0x02, 0, 0x5E, 0x10, 0x44, 0xC0],
            fw_version: ShortStr::new(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms,
            node_board: NodeBoard::NoCarrier,
            radio: None,
            image: None,
        }
    }

    fn rx() -> BridgeToHost {
        let body = FromNodeBody::from_slice(b"body").unwrap();
        BridgeToHost::FromNode { slot: Slot::new(0, 0).unwrap(), rx_us: 1, kind: 1, body }
    }

    #[test]
    fn gate_drops_frames_when_they_decode_before_first_ready() {
        let mut gate = Gate::new();
        let now = Instant::now();
        assert_eq!(gate.admit(rx(), now), Admitted::Stale);
        assert_eq!(gate.admit(rx(), now), Admitted::Stale);
        assert_eq!(gate.skipped(), 2);
        assert!(!gate.announced());
        assert!(matches!(
            gate.admit(ready(500), now),
            Admitted::Ready { first: true, fresh: true, .. }
        ));
        assert_eq!(gate.admit(rx(), now), Admitted::Message(rx()));
    }

    #[test]
    fn gate_reports_ready_unfresh_when_same_life_answers_twice() {
        let mut gate = Gate::new();
        let t0 = Instant::now();
        gate.admit(ready(1_200), t0);
        let again = gate.admit(ready(1_202), t0 + Duration::from_millis(2));
        assert!(matches!(again, Admitted::Ready { first: false, fresh: false, .. }));
    }

    #[test]
    fn gate_reports_fresh_life_when_uptime_falls() {
        let mut gate = Gate::new();
        let t0 = Instant::now();
        gate.admit(ready(50_000), t0);
        let next = gate.admit(ready(120), t0 + Duration::from_secs(1));
        assert!(matches!(next, Admitted::Ready { first: false, fresh: true, .. }));
    }

    #[test]
    fn gate_reports_same_life_fresh_and_drops_until_it_when_restarted() {
        // A Bluetooth central handed the queue back gets a `Ready` from the life it already
        // reported, and it is a new start of the stream all the same.
        let mut gate = Gate::new();
        let t0 = Instant::now();
        gate.admit(ready(10_000), t0);
        gate.restart();
        assert_eq!(gate.admit(rx(), t0), Admitted::Stale);
        let back = gate.admit(ready(30_000), t0 + Duration::from_secs(20));
        assert!(matches!(back, Admitted::Ready { first: true, fresh: true, .. }));
    }
}
