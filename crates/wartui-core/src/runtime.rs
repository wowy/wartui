//! Wiring: link in, engine in the middle, store and UI out.
//!
//! The only place that reads a clock, and the only place that decides *when* things
//! happen; [`crate::engine::FleetEngine`] decides what happens.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::LinkHandle;

use crate::engine::{Command, Event, FleetEngine, Now, Snapshot};
use crate::store::Store;

/// How often the engine ages liveness and republishes the snapshot.
///
/// Four times a second: fast enough that a node going quiet is noticed while the
/// operator is still looking, slow enough not to redraw per observation.
pub const TICK: Duration = Duration::from_millis(250);

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
/// be a complete database.
pub async fn drive(
    mut link: LinkHandle,
    store: Store,
    mut engine: FleetEngine,
    snapshot: watch::Sender<Arc<Snapshot>>,
    mut commands: mpsc::Receiver<Command>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Once the UI is gone this branch is disabled rather than polled. A closed
    // receiver is permanently ready, so leaving it in the `select!` would spin
    // the loop as fast as the scheduler allows for the rest of the capture.
    let mut steerable = true;

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

        if publish {
            // Lossy on purpose: the UI wants the latest state, never a queue of
            // stale ones, and a UI that has stopped reading must not be able to
            // slow the engine down.
            let _ = snapshot.send(Arc::new(engine.snapshot(now, store.stats())));
        }
    }

    let _ = snapshot.send(Arc::new(engine.snapshot(now(), store.stats())));
    store.close();
}
