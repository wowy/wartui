//! Wiring: link in, engine in the middle, store and UI out.
//!
//! The only place that reads a clock and decides *when* things happen.
//! [`crate::engine::FleetEngine`] decides what happens. It reads tokio's clock, so a paused test
//! drives the engine and the simulator on one clock, as `FleetEngine::note_arrival` needs to
//! compare bridge-elapsed with host-elapsed time.
//!
//! **The bridge's GPS bytes stop here.** They go to the bridge's receiver in the position chain
//! ([`crate::gps::Gps::feed_bytes`]) and never reach the engine, which reads positions through the
//! chain like any other. While the connected bridge's receiver is sending sentences, the host's
//! puck search stands down (`stand_host_down`); it resumes when the last one ages past the limit
//! or the bridge disconnects. Whether the connected bridge has a GPS port is the engine's to say
//! ([`FleetEngine::bridge_has_gps`]), read after the engine has seen each link event. Each
//! connection, and each disconnect, starts the bridge's stream over.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::air::{Frame, OtaStatusMsg};
use wartui_proto::link::{BridgeToHost, HostToBridge, PanelLines};
use wartui_proto::mac;

use crate::engine::{Command, Event, FleetEngine, HostSample, Now, Snapshot};
use crate::ota::{Failure, OtaEvent, OtaSession, Outcome, Progress};
use crate::record::{CaptureSettings, Record};
use crate::store::{Store, StoreReport};

/// How often the engine ages liveness and republishes the snapshot.
///
/// Four times a second: fast enough that a node going quiet is noticed while the
/// operator is still looking, slow enough not to redraw per observation.
pub const TICK: Duration = Duration::from_millis(250);

/// How often the bridge's panel is repainted, when it has one.
///
/// One second, four ticks: fast enough to watch a node drop while standing over the dongle, slow
/// enough that estimator counts do not flicker. A ceiling, not a cadence: an unchanged push is not
/// sent, so a settled capture sends little.
pub const PANEL_INTERVAL: Duration = Duration::from_millis(1_000);

/// How long the panel may go without a push, however little has changed.
///
/// Suppressing unchanged pushes keeps a quiet capture off the wire, but makes the host's record of
/// what the dongle shows load-bearing, and only a `Ready` corrects it. A bridge that reboots
/// mid-session comes up on its fallback screen. If its `Ready` is lost, the lines never change,
/// nothing is pushed, and the dongle says "no host" with a host attached. So the whole panel goes
/// out on this interval regardless. Every frame is idempotent, so resynchronizing is sending one.
/// The bridge redraws only rows that differ, so an unneeded repaint costs one frame and no SPI.
pub const PANEL_REPAINT: Duration = Duration::from_secs(10);

/// How often the host records its own state as a `host_status` row.
///
/// Five seconds, the bridge's status poll (`EngineConfig::status_interval`), so both timelines
/// share a resolution. On its own timer rather than the poll's replies, so the host keeps recording
/// while the bridge is gone, when a sagging supply matters most.
pub const HOST_SAMPLE: Duration = Duration::from_secs(5);

/// The current time, in both forms the engine needs.
#[must_use]
pub fn now() -> Now {
    Now {
        mono: tokio::time::Instant::now().into_std(),
        unix_ms: chrono::Utc::now().timestamp_millis(),
    }
}

/// How many operator instructions may be waiting at once.
///
/// Small on purpose: a queue deeper than the operator's patience would replay a burst
/// of assignments minutes after they stopped pressing the key.
pub const COMMAND_QUEUE: usize = 8;

/// Run the fleet until the link closes or `stop` fires.
///
/// Consumes the store so the last batch commits and `ended_at` is written before returning: an
/// interrupted capture is still a complete database. Returns what the store's writer did, for
/// `wartui bench`.
pub async fn drive(
    mut link: LinkHandle,
    store: Store,
    mut engine: FleetEngine,
    snapshot: watch::Sender<Arc<Snapshot>>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: oneshot::Receiver<()>,
) -> StoreReport {
    let mut settings = SettingsLog::default();
    settings.record(&store, engine.capture_settings(), now());
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut host_ticker = tokio::time::interval(HOST_SAMPLE);
    host_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is immediate, so a capture opens on a host row that `analyze` reads
    // the rest against, and its figures cover the capture from the start.
    let sample = |store: &Store| HostSample {
        store: store.stats(),
        peaks: store.take_peaks(),
        health: crate::health::read(),
    };
    // Once the UI is gone this branch is disabled. A closed receiver is always ready, and would
    // spin the loop for the rest of the capture.
    let mut steerable = true;
    // What the panel was last sent, and when. The interval keeps a busy fleet from repainting per
    // command. Comparing lines keeps a quiet one from resending the same frame.
    let mut panel_sent: Option<Instant> = None;
    let mut panel_drawn: Option<Instant> = None;
    let mut panel_lines: Option<PanelLines> = None;
    let bridge_gps = engine.position().bridge_gps().cloned();
    let host_gps = engine.position().gps().cloned();

    loop {
        let event = tokio::select! {
            biased;
            _ = &mut stop => break,
            // Ahead of the ticker, so pressing a key does not wait out a tick
            // and then miss the heartbeat it was meant to catch.
            command = commands.recv(), if steerable => match command {
                Some(command) => Event::Command(command),
                // The UI has gone. The capture carries on to the end, unsteered.
                None => {
                    steerable = false;
                    continue;
                }
            },
            _ = ticker.tick() => Event::Tick,
            _ = host_ticker.tick() => Event::HostSample(sample(&store)),
            event = link.recv() => match event {
                Some(event) => Event::Link(event),
                // The transport gave up. A cable falling out is different: it surfaces as
                // `Disconnected` and reconnects.
                None => break,
            },
        };

        match &event {
            // The engine ignores these, so what it last said about the bridge still holds.
            Event::Link(LinkEvent::Message(BridgeToHost::Gps { bytes })) => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                if let Some(gps) = &bridge_gps {
                    gps.feed_bytes(bytes, now_ms);
                }
                let has_gps = engine.bridge_has_gps();
                stand_host_down(host_gps.as_ref(), bridge_gps.as_ref(), has_gps, now_ms);
                continue;
            }
            Event::Link(LinkEvent::Connected(_) | LinkEvent::Disconnected { .. }) => {
                if let Some(gps) = &bridge_gps {
                    gps.reset_stream();
                }
            }
            // A quiet receiver sends nothing to age its fix by, nor to resume the host's search.
            Event::Tick => {
                if let Some(gps) = &bridge_gps {
                    gps.tick(chrono::Utc::now().timestamp_millis());
                }
            }
            _ => {}
        }
        // The events that re-decide the stand-down, once the engine has seen them, so a connect or
        // disconnect counts.
        let restands = matches!(
            event,
            Event::Tick | Event::Link(LinkEvent::Connected(_) | LinkEvent::Disconnected { .. })
        );

        let publish = matches!(
            event,
            Event::Tick
                | Event::Command(_)
                | Event::Link(
                    wartui_bridge::LinkEvent::Connected(_)
                        | wartui_bridge::LinkEvent::Disconnected { .. }
                )
        );
        // A bridge that just announced itself shows its no-host screen, and a reboot does not
        // re-enumerate USB. Without this, the cached lines would suppress the push that takes the
        // panel back.
        let relinked = matches!(event, Event::Link(wartui_bridge::LinkEvent::Connected(_)));

        let now = now();
        let batch = engine.handle(event, now);
        settings.record(&store, engine.capture_settings(), now);
        if restands {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let has_gps = engine.bridge_has_gps();
            stand_host_down(host_gps.as_ref(), bridge_gps.as_ref(), has_gps, now_ms);
        }

        if !batch.records.is_empty() {
            store.submit(batch.records);
        }
        for cmd in batch.urgent {
            if let Err(e) = link.send_urgent(cmd) {
                tracing::warn!("dropping an urgent command: {e}");
            }
        }
        for cmd in batch.bulk {
            if let Err(e) = link.send_bulk(cmd) {
                tracing::debug!("dropping a bulk command: {e}");
            }
        }

        if relinked {
            panel_lines = None;
        }

        if publish {
            let view = Arc::new(engine.snapshot(now, store.stats()));
            // Lossy on purpose: the UI wants the latest state, and one that stopped reading must
            // not slow the engine.
            let _ = snapshot.send(Arc::clone(&view));

            // Composed here, not in `engine::handle`: a screen is not a fleet decision, and the
            // engine reads no clock. A bridge with no panel is sent nothing, which removes the
            // operator flag.
            if let Some(geometry) = view.bridge.as_ref().and_then(|bridge| bridge.panel) {
                let due =
                    panel_sent.is_none_or(|last| now.mono.duration_since(last) >= PANEL_INTERVAL);
                let lines = due.then(|| crate::panel::render(&view, geometry));
                // Interval first, so an unchanged panel costs a render, not a frame, and a changed
                // one waits its turn.
                if let Some(lines) = lines {
                    panel_sent = Some(now.mono);
                    // The lines moved, or the host should stop trusting its record of the glass
                    // (`PANEL_REPAINT`).
                    let stale = panel_drawn
                        .is_none_or(|last| now.mono.duration_since(last) >= PANEL_REPAINT);
                    if stale || panel_lines.as_ref() != Some(&lines) {
                        // At debug, and only on a send, so a capture's log shows what the dongle
                        // showed: one line per change or repaint.
                        tracing::debug!(
                            rows = lines.len(),
                            "pushing the panel: {}",
                            lines
                                .iter()
                                .map(|line| line.text.as_str())
                                .collect::<Vec<_>>()
                                .join(" | ")
                        );
                        match link.send_bulk(HostToBridge::ShowPanel { lines: lines.clone() }) {
                            // Cached only once really sent. A dropped push that still updated the
                            // cache would be suppressed forever by the comparison above.
                            Ok(()) => {
                                panel_lines = Some(lines);
                                panel_drawn = Some(now.mono);
                            }
                            Err(e) => tracing::debug!("dropping a panel push: {e}"),
                        }
                    }
                }
            }
        }
    }

    // Before the last host row, so its drop count includes a lost settings change.
    settings.finish(&store);
    // So a capture always ends on a host row holding its final counts.
    let now = now();
    store.submit(engine.handle(Event::HostSample(sample(&store)), now).records);
    let _ = snapshot.send(Arc::new(engine.snapshot(now, store.stats())));
    store.close()
}

/// Stand the host's receiver down while the connected bridge has a GPS port and its receiver is
/// sending sentences ([`crate::gps::Gps::is_talking`]), and let it search otherwise.
///
/// Keyed on sentences, fix or not, so the operator can watch the bridge's receiver acquire. A
/// GPS-built bridge whose receiver is unplugged or silent leaves the search running.
/// [`crate::gps::Gps::set_suspended`] leaves a port named with `--gps` alone.
fn stand_host_down(
    host: Option<&crate::gps::Gps>,
    bridge: Option<&crate::gps::Gps>,
    bridge_has_gps: bool,
    now_ms: i64,
) {
    if let Some(host) = host {
        host.set_suspended(bridge_has_gps && bridge.is_some_and(|gps| gps.is_talking(now_ms)));
    }
}

/// How often [`drive_ota`] ticks its session: well inside its 500 ms status timeout.
pub const OTA_TICK: Duration = Duration::from_millis(50);

/// Run one node's update to its end, or until `stop` resolves, which cancels it.
///
/// Frames from other nodes are handed to the session, which reads only its own node's.
/// `progress` is called after every event, for showing.
///
/// Every frame sent, every send result, every status from the node and every phase change is
/// logged at debug level with structured fields, so a bench log of an update can be read back
/// by a script.
pub async fn drive_ota(
    link: &mut LinkHandle,
    session: &mut OtaSession,
    stop: impl std::future::Future<Output = ()>,
    mut progress: impl FnMut(&Progress),
) -> Outcome {
    tokio::pin!(stop);
    let mut ticker = tokio::time::interval(OTA_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut phase = session.progress().phase;
    tracing::debug!(node = %mac::full(&session.node()), phase = ?phase, "ota phase");
    loop {
        let sends = tokio::select! {
            () = &mut stop => session.cancel(now()),
            _ = ticker.tick() => session.handle(now(), OtaEvent::Tick),
            event = link.recv() => match event {
                Some(LinkEvent::Message(BridgeToHost::SendResult { id, status, tx_us })) => {
                    tracing::debug!(id, status = ?status, tx_us, "ota send result");
                    session.handle(now(), OtaEvent::SendResult { id, status })
                }
                Some(LinkEvent::Message(BridgeToHost::Rx { src, payload, .. })) => {
                    if src == session.node()
                        && tracing::enabled!(tracing::Level::DEBUG)
                        && let Ok(status) = OtaStatusMsg::decode(&payload)
                    {
                        tracing::debug!(
                            session = status.session,
                            state = ?status.state,
                            next_offset = status.next_offset,
                            "ota status"
                        );
                    }
                    session.handle(now(), OtaEvent::Rx { src, payload: &payload })
                }
                Some(_) => Vec::new(),
                None => return Outcome::Failed(Failure::LinkClosed),
            },
        };
        for cmd in sends {
            if tracing::enabled!(tracing::Level::DEBUG)
                && let HostToBridge::SendEspNow { id, payload, .. } = &cmd
            {
                let (kind, offset) = match Frame::decode(payload) {
                    Ok(Frame::OtaBegin(_)) => ("begin", None),
                    Ok(Frame::OtaChunk(chunk)) => ("chunk", Some(chunk.offset)),
                    Ok(Frame::OtaCommit(_)) => ("commit", None),
                    Ok(Frame::OtaAbort(_)) => ("abort", None),
                    Ok(Frame::OtaConfirm(_)) => ("confirm", None),
                    _ => ("other", None),
                };
                tracing::debug!(id, kind, offset, len = payload.len(), "ota send");
            }
            if let Err(e) = link.send_urgent(cmd) {
                // The session's own timeouts resend it.
                tracing::warn!("dropping an update frame: {e}");
            }
        }
        let now_progress = session.progress();
        if now_progress.phase != phase {
            phase = now_progress.phase;
            tracing::debug!(
                node = %mac::full(&session.node()),
                phase = ?phase,
                acked = now_progress.acked,
                "ota phase"
            );
        }
        progress(&now_progress);
        if let Some(outcome) = session.outcome() {
            tracing::debug!(outcome = ?outcome, "ota outcome");
            return outcome.clone();
        }
    }
}

/// Applied settings as `capture_settings` rows: applied rather than requested, because clamps and
/// no-ops belong to the engine.
///
/// A change takes its sequence number and time when first seen, and a full queue retries that same
/// row on later passes without blocking the engine. A refused retry is not a drop. A change is
/// counted as one store drop only once it is lost: replaced by a different change before it was
/// written, or still unwritten when the run ends. The sequence gap and that one drop expose the
/// missing transition; neither claims adoption.
#[derive(Debug, Default)]
struct SettingsLog {
    /// The last settings written.
    written: Option<CaptureSettings>,
    /// A change seen but not yet accepted by the store: its sequence, time and settings.
    pending: Option<(u64, i64, CaptureSettings)>,
    /// The sequence number the next change takes.
    next_sequence: u64,
}

impl SettingsLog {
    /// Offer the current settings once per pass, if they differ from the last written.
    fn record(&mut self, store: &Store, settings: CaptureSettings, now: Now) {
        if let Some((_, _, waiting)) = self.pending
            && waiting != settings
        {
            self.pending = None;
            store.count_dropped(1);
        }
        if self.pending.is_none() {
            if self.written == Some(settings) {
                return;
            }
            self.pending = Some((self.next_sequence, now.unix_ms, settings));
            self.next_sequence += 1;
        }
        let Some((sequence, at_ms, settings)) = self.pending else { return };
        if store.offer(Record::CaptureSettings { sequence, at_ms, settings }) {
            self.written = Some(settings);
            self.pending = None;
        }
    }

    /// Count a change still unwritten when the run ends as one drop.
    fn finish(self, store: &Store) {
        if self.pending.is_some() {
            store.count_dropped(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gps::Gps;
    use crate::position::DEFAULT_MAX_AGE;

    const GSV: &[u8] = b"$GPGSV,1,1,00*79\r\n";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76\r\n";

    #[test]
    fn runtime_suspends_host_gps_when_bridge_sends_no_fix_sentences() {
        // No sky: sentences every epoch, and the operator watches the bridge's receiver acquire.
        let (host, bridge) = (Gps::detached(), Gps::bridged(DEFAULT_MAX_AGE));
        stand_host_down(Some(&host), Some(&bridge), true, 1_000);
        assert!(!host.suspended(), "a GPS-built bridge whose receiver says nothing");
        bridge.feed_bytes(GGA_NO_FIX, 1_000);
        bridge.feed_bytes(GSV, 1_000);
        stand_host_down(Some(&host), Some(&bridge), true, 1_000);
        assert!(host.suspended());
    }

    #[test]
    fn runtime_suspends_host_gps_when_bridge_sends_sentences() {
        let (host, bridge) = (Gps::detached(), Gps::bridged(DEFAULT_MAX_AGE));
        bridge.feed_bytes(GSV, 1_000);
        stand_host_down(Some(&host), Some(&bridge), false, 1_000);
        assert!(!host.suspended(), "sentences on a bridge without a GPS port");
        stand_host_down(Some(&host), Some(&bridge), true, 1_000);
        assert!(host.suspended());
    }

    #[test]
    fn runtime_resumes_host_gps_when_bridge_sentences_go_stale() {
        let (host, bridge) = (Gps::detached(), Gps::bridged(DEFAULT_MAX_AGE));
        bridge.feed_bytes(GSV, 1_000);
        stand_host_down(Some(&host), Some(&bridge), true, 6_000);
        assert!(host.suspended());
        stand_host_down(Some(&host), Some(&bridge), true, 6_001);
        assert!(!host.suspended());
    }

    #[test]
    fn runtime_keeps_host_gps_searching_when_bridge_reconnects_until_a_new_sentence() {
        let (host, bridge) = (Gps::detached(), Gps::bridged(DEFAULT_MAX_AGE));
        bridge.feed_bytes(GSV, 1_000);
        // What `drive` does on `Connected`: the old sentence is within the age limit, but from
        // before.
        bridge.reset_stream();
        stand_host_down(Some(&host), Some(&bridge), true, 2_000);
        assert!(!host.suspended());
        bridge.feed_bytes(GSV, 3_000);
        stand_host_down(Some(&host), Some(&bridge), true, 3_000);
        assert!(host.suspended());
    }
    use crate::engine::EngineConfig;
    use crate::store::{CaptureInfo, StoreConfig, open_readonly};
    use wartui_proto::plan::ChannelPool;

    #[test]
    fn runtime_records_settings_when_applied_values_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("settings.db");
        let clock = Now { mono: Instant::now(), unix_ms: 1000 };
        let mut engine = FleetEngine::new(EngineConfig::default(), clock);
        let store = Store::create(&StoreConfig::new(&path), &CaptureInfo::default(), clock.unix_ms)
            .expect("store");
        let mut log = SettingsLog::default();
        log.record(&store, engine.capture_settings(), clock);
        log.record(&store, engine.capture_settings(), clock);
        assert_eq!(log.next_sequence, 1, "unchanged settings are not per-tick records");

        engine.handle(Event::Command(Command::SetTxPower { nodes: 127, bridge: -1 }), clock);
        log.record(&store, engine.capture_settings(), clock);
        // A repeated request is a no-op even when the supplied values need clamping.
        engine.handle(Event::Command(Command::SetTxPower { nodes: 127, bridge: -1 }), clock);
        log.record(&store, engine.capture_settings(), clock);
        assert_eq!(log.next_sequence, 2);

        let reversed = Now { unix_ms: 999, ..clock };
        engine.handle(Event::Command(Command::SetPool { pool: ChannelPool::Us }), reversed);
        log.record(&store, engine.capture_settings(), reversed);
        engine.handle(Event::Command(Command::RememberBle { on: false }), reversed);
        log.record(&store, engine.capture_settings(), reversed);
        assert_eq!(log.next_sequence, 4);
        log.finish(&store);
        assert_eq!(store.close().dropped, 0);
        let conn = open_readonly(&path).expect("read-only");
        let mut stmt = conn
            .prepare("SELECT v FROM kv WHERE k LIKE 'capture.settings.%' ORDER BY k")
            .expect("settings query");
        let values = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("settings")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        assert_eq!(values.len(), 4);
        assert!(values[0].contains("nodes_tx_power_quarter_dbm=8;"));
        assert!(values[1].contains("nodes_tx_power_quarter_dbm=80;bridge_tx_power_quarter_dbm=8;"));
        assert!(values[2].contains("at_ms=999;pool=us;"));
        assert!(values[3].contains("remember_ble=false;"));
    }

    /// A store whose one-slot queue is full, and the receiver that frees it.
    fn full_store(clock: Now) -> (Store, std::sync::mpsc::Receiver<Record>) {
        let (store, rx) = Store::detached(1);
        assert!(store.offer(Record::CaptureSettings {
            sequence: u64::MAX,
            at_ms: clock.unix_ms,
            settings: settings(clock),
        }));
        (store, rx)
    }

    fn settings(clock: Now) -> CaptureSettings {
        FleetEngine::new(EngineConfig::default(), clock).capture_settings()
    }

    /// Everything the store accepted after the filler, in order.
    fn written(rx: &std::sync::mpsc::Receiver<Record>) -> Vec<(u64, i64, CaptureSettings)> {
        rx.try_iter()
            .filter_map(|record| match record {
                Record::CaptureSettings { sequence, at_ms, settings } if sequence != u64::MAX => {
                    Some((sequence, at_ms, settings))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn runtime_writes_settings_once_without_drops_when_retry_succeeds() {
        let clock = Now { mono: Instant::now(), unix_ms: 1000 };
        let (store, rx) = full_store(clock);
        let applied = settings(clock);
        let mut log = SettingsLog::default();
        for pass in 0..5 {
            log.record(&store, applied, Now { unix_ms: 1000 + pass, ..clock });
        }
        assert_eq!(store.stats().dropped, 0, "a refused retry is not a drop");
        assert_eq!(log.next_sequence, 1, "retries reuse the sequence number");

        let _filler = rx.try_recv().expect("filler");
        log.record(&store, applied, Now { unix_ms: 2000, ..clock });
        log.record(&store, applied, Now { unix_ms: 2001, ..clock });
        log.finish(&store);
        assert_eq!(written(&rx), vec![(0, 1000, applied)], "first sighting's time is kept");
        assert_eq!(store.close().dropped, 0);
    }

    #[test]
    fn runtime_counts_one_drop_and_leaves_gap_when_unwritten_change_is_superseded() {
        let clock = Now { mono: Instant::now(), unix_ms: 1000 };
        let (store, rx) = full_store(clock);
        let first = settings(clock);
        let second = CaptureSettings { tx_power: first.tx_power + 4, ..first };
        let mut log = SettingsLog::default();
        for _ in 0..3 {
            log.record(&store, first, clock);
        }

        let _filler = rx.try_recv().expect("filler");
        let later = Now { unix_ms: 2000, ..clock };
        log.record(&store, second, later);
        log.record(&store, second, later);
        log.finish(&store);
        assert_eq!(written(&rx), vec![(1, 2000, second)], "sequence 0 is the gap");
        assert_eq!(store.close().dropped, 1);
    }

    #[test]
    fn runtime_counts_one_drop_when_change_is_unwritten_at_finish() {
        let clock = Now { mono: Instant::now(), unix_ms: 1000 };
        let (store, rx) = full_store(clock);
        let mut log = SettingsLog::default();
        for _ in 0..10 {
            log.record(&store, settings(clock), clock);
        }
        log.finish(&store);
        assert!(written(&rx).is_empty());
        assert_eq!(store.close().dropped, 1);
    }
}
