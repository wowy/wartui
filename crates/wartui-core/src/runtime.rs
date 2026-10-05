//! Wiring: link in, engine in the middle, store and UI out.
//!
//! The only place that reads a clock and decides *when* things happen.
//! [`crate::engine::FleetEngine`] decides what happens. It reads tokio's clock, so a paused test
//! drives the engine and the simulator on one clock, as `FleetEngine::note_arrival` needs to
//! compare bridge-elapsed with host-elapsed time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::LinkHandle;
use wartui_proto::link::{HostToBridge, PanelLines};

use crate::engine::{Command, Event, FleetEngine, HostSample, Now, Snapshot};
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
/// out on this interval regardless. Every frame is idempotent, so resynchronising is sending one.
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
    let mut settings_recorded: Option<CaptureSettings> = None;
    let mut settings_sequence = 0_u64;
    record_settings(&store, &mut settings_recorded, &mut settings_sequence, &engine, now());
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
        record_settings(&store, &mut settings_recorded, &mut settings_sequence, &engine, now);

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
                            // cache would be suppressed for ever by the comparison above.
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

    // So a capture always ends on a host row holding its final counts.
    let now = now();
    store.submit(engine.handle(Event::HostSample(sample(&store)), now).records);
    let _ = snapshot.send(Arc::new(engine.snapshot(now, store.stats())));
    store.close()
}

/// Record applied settings rather than requested commands: clamps and no-ops belong to the
/// engine. Retry the latest snapshot after a full queue without ever blocking the engine. A
/// sequence gap and store drop count expose the missing transition; neither claims adoption.
fn record_settings(
    store: &Store,
    recorded: &mut Option<CaptureSettings>,
    sequence: &mut u64,
    engine: &FleetEngine,
    now: Now,
) {
    let settings = engine.capture_settings();
    if *recorded == Some(settings) {
        return;
    }
    let record = Record::CaptureSettings { sequence: *sequence, at_ms: now.unix_ms, settings };
    *sequence += 1;
    if store.submit(vec![record]) == 0 {
        *recorded = Some(settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut recorded = None;
        let mut sequence = 0;
        record_settings(&store, &mut recorded, &mut sequence, &engine, clock);
        record_settings(&store, &mut recorded, &mut sequence, &engine, clock);
        assert_eq!(sequence, 1, "unchanged settings are not per-tick records");

        engine.handle(Event::Command(Command::SetTxPower { nodes: 127, bridge: -1 }), clock);
        record_settings(&store, &mut recorded, &mut sequence, &engine, clock);
        // A repeated request is a no-op even when the supplied values need clamping.
        engine.handle(Event::Command(Command::SetTxPower { nodes: 127, bridge: -1 }), clock);
        record_settings(&store, &mut recorded, &mut sequence, &engine, clock);
        assert_eq!(sequence, 2);

        let reversed = Now { unix_ms: 999, ..clock };
        engine.handle(Event::Command(Command::SetPool { pool: ChannelPool::Us }), reversed);
        record_settings(&store, &mut recorded, &mut sequence, &engine, reversed);
        engine.handle(Event::Command(Command::RememberBle { on: false }), reversed);
        record_settings(&store, &mut recorded, &mut sequence, &engine, reversed);
        assert_eq!(sequence, 4);
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
}
