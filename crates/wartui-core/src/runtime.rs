//! Wiring: link in, engine in the middle, store and UI out.
//!
//! The only place that reads a clock, and the only place that decides *when* things
//! happen; [`crate::engine::FleetEngine`] decides what happens.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::LinkHandle;
use wartui_proto::link::{HostToBridge, PanelLines};

use crate::engine::{Command, Event, FleetEngine, Now, Snapshot};
use crate::store::{Store, StoreReport};

/// How often the engine ages liveness and republishes the snapshot.
///
/// Four times a second: fast enough that a node going quiet is noticed while the
/// operator is still looking, slow enough not to redraw per observation.
pub const TICK: Duration = Duration::from_millis(250);

/// How often the bridge's panel is repainted, when it has one.
///
/// One second, which is four ticks: fast enough to watch a node drop while standing
/// over the dongle, slow enough that the estimator counts do not flicker between two
/// readings of the same number. A push carries every line, so the rate is also the
/// whole of the resynchronisation protocol — a bridge that rebooted a moment ago is
/// correct again within one of these.
pub const PANEL_INTERVAL: Duration = Duration::from_millis(1_000);

/// The current time, in both forms the engine needs.
#[must_use]
pub fn now() -> Now {
    Now { mono: Instant::now(), unix_ms: chrono::Utc::now().timestamp_millis() }
}

/// How many operator instructions may be waiting at once.
///
/// Small on purpose: a queue deeper than the operator's patience would replay a burst
/// of assignments minutes after they stopped pressing the key.
pub const COMMAND_QUEUE: usize = 8;

/// Run the fleet until the link closes or `stop` fires.
///
/// Consumes the store so the last batch is committed and the session's
/// `ended_at` written before this returns — an interrupted capture should still
/// be a complete database. Returns what the store's writer did, which `wartui bench`
/// reports and nothing else reads.
pub async fn drive(
    mut link: LinkHandle,
    store: Store,
    mut engine: FleetEngine,
    snapshot: watch::Sender<Arc<Snapshot>>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: oneshot::Receiver<()>,
) -> StoreReport {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Once the UI is gone this branch is disabled rather than polled. A closed
    // receiver is permanently ready, so leaving it in the `select!` would spin
    // the loop as fast as the scheduler allows for the rest of the capture.
    let mut steerable = true;
    // What the panel was last sent, and when. Both are needed: the interval keeps a
    // busy fleet from repainting on every command, and comparing the lines keeps a
    // quiet one from sending a frame a second that says exactly what the last one did.
    let mut panel_sent: Option<Instant> = None;
    let mut panel_lines: Option<PanelLines> = None;

    loop {
        let event = tokio::select! {
            biased;
            _ = &mut stop => break,
            // Ahead of the ticker, so pressing a key does not wait out a tick
            // and then miss the heartbeat it was meant to catch.
            command = commands.recv(), if steerable => match command {
                Some(command) => Event::Command(command),
                // The UI has gone. The capture carries on to the end of its
                // batch; there is simply nobody left to steer it.
                None => {
                    steerable = false;
                    continue;
                }
            },
            _ = ticker.tick() => Event::Tick,
            event = link.recv() => match event {
                Some(event) => Event::Link(event),
                // The transport gave up entirely, which is different from a
                // cable falling out: that surfaces as `Disconnected` and
                // reconnects on its own.
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
        // A bridge that has just announced itself is showing whatever it draws with no
        // host, and a reboot does not re-enumerate the USB device — so without this the
        // cached lines below would suppress the very push that takes the panel back.
        let relinked = matches!(event, Event::Link(wartui_bridge::LinkEvent::Connected(_)));

        let now = now();
        let batch = engine.handle(event, now);

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
            // Lossy on purpose: the UI wants the latest state, never a queue of
            // stale ones, and a UI that has stopped reading must not be able to
            // slow the engine down.
            let _ = snapshot.send(Arc::clone(&view));

            // The panel is a view of the same snapshot, so it is composed here rather
            // than in `engine::handle`: a screen is not a fleet decision, and the
            // engine reads no clock. A bridge that announced no panel is sent nothing
            // at all, which is what removes the operator flag.
            if let Some(geometry) = view.bridge.as_ref().and_then(|bridge| bridge.panel) {
                let due =
                    panel_sent.is_none_or(|last| now.mono.duration_since(last) >= PANEL_INTERVAL);
                let lines = due.then(|| crate::panel::render(&view, geometry));
                // Against the interval first, so an unchanged panel costs a render
                // rather than a frame, and a changed one still waits its turn.
                if let Some(lines) = lines {
                    panel_sent = Some(now.mono);
                    if panel_lines.as_ref() != Some(&lines) {
                        // At debug and only on a change, so a log of a capture shows
                        // what the dongle was showing at each point in it rather than
                        // a line a second saying the same thing.
                        tracing::debug!(
                            rows = lines.len(),
                            "pushing the panel: {}",
                            lines
                                .iter()
                                .map(|line| line.text.as_str())
                                .collect::<Vec<_>>()
                                .join(" | ")
                        );
                        if let Err(e) =
                            link.send_bulk(HostToBridge::ShowPanel { lines: lines.clone() })
                        {
                            tracing::debug!("dropping a panel push: {e}");
                        }
                        panel_lines = Some(lines);
                    }
                }
            }
        }
    }

    let _ = snapshot.send(Arc::new(engine.snapshot(now(), store.stats())));
    store.close()
}
