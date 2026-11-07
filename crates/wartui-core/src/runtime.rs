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
//! or the bridge disconnects. Whether a bridge is connected is the engine's to say
//! ([`FleetEngine::bridge_connected`]), read after the engine has seen each link event. Each
//! connection, and each disconnect, starts the bridge's stream over.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::bus::{DownMsg, UpType, UpdateStatus};
use wartui_proto::link::{BridgeToHost, HostToBridge, UpdateOp};
use wartui_proto::mac;
use wartui_proto::radio_link::update::{self, OP_MAX, Op, Sender, Stage};

use crate::engine::{Command, Event, FleetEngine, HostSample, Now, Snapshot};
use crate::ota::{Failure, OtaEvent, OtaImage, OtaSession, Outcome, Progress};
use crate::record::{CaptureSettings, Record};
use crate::store::{Store, StoreReport};

/// How often the engine ages liveness and republishes the snapshot.
///
/// Four times a second: fast enough that a node going quiet is noticed while the
/// operator is still looking, slow enough not to redraw per observation.
pub const TICK: Duration = Duration::from_millis(250);

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
                let connected = engine.bridge_connected();
                stand_host_down(host_gps.as_ref(), bridge_gps.as_ref(), connected, now_ms);
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

        let now = now();
        let batch = engine.handle(event, now);
        settings.record(&store, engine.capture_settings(), now);
        if restands {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let connected = engine.bridge_connected();
            stand_host_down(host_gps.as_ref(), bridge_gps.as_ref(), connected, now_ms);
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

        if publish {
            let view = Arc::new(engine.snapshot(now, store.stats()));
            // Lossy on purpose: the UI wants the latest state, and one that stopped reading must
            // not slow the engine.
            let _ = snapshot.send(view);
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

/// Stand the host's receiver down while a bridge is connected and its receiver is
/// sending sentences ([`crate::gps::Gps::is_talking`]), and let it search otherwise.
///
/// Keyed on sentences, fix or not, so the operator can watch the bridge's receiver acquire. A
/// bridge whose receiver is unplugged or silent leaves the search running.
/// [`crate::gps::Gps::set_suspended`] leaves a port named with `--gps` alone.
fn stand_host_down(
    host: Option<&crate::gps::Gps>,
    bridge: Option<&crate::gps::Gps>,
    bridge_connected: bool,
    now_ms: i64,
) {
    if let Some(host) = host {
        host.set_suspended(bridge_connected && bridge.is_some_and(|gps| gps.is_talking(now_ms)));
    }
}

/// How often [`drive_fleet`] ticks its sessions: well inside their 500 ms status timeout.
pub const OTA_TICK: Duration = Duration::from_millis(50);

/// What [`drive_fleet`] reports as it goes, for showing.
#[derive(Debug, Clone, Copy)]
pub enum FleetReport<'a> {
    /// The P4 checked the image uploaded into its cache, this long after the drive started.
    Cached(Duration),
    /// Session `index` stands at `progress`.
    Session(usize, &'a Progress),
}

/// Run every node's update at once to its end, or until `stop` resolves, which cancels them all.
/// Returns each session's outcome, in the order given.
///
/// `image`, when given, is uploaded into the P4's node image cache beside the sessions' `begin`s,
/// and the sessions are told once the P4 has checked it ([`OtaEvent::Cached`]); none sends a
/// chunk before. Without one, nothing is uploaded and no session is told, so pass it whenever a
/// session streams. An upload the P4 refuses or loses cancels every session, which sends each its
/// abort, and is the error. A `Begin` for the image the cache already holds is answered `Verified`
/// at once, and the upload ends there.
///
/// Every event goes to every session still running, and each reads only its own: its node's
/// records, and the delivery answers to its own ids, which no two sessions share
/// ([`crate::ota::Ids`]). A session that fails leaves the rest running. A cancel still sends each
/// session's abort. `report` is called for every session after every event, and once when the
/// cache holds the image, for showing.
///
/// Every record sent, every delivery answer, every status from a node, every phase change and the
/// upload's end are logged at debug level with structured fields, so a bench log of an update
/// can be read back by a script.
///
/// # Errors
/// Why the P4 refused or lost the upload.
pub async fn drive_fleet(
    link: &mut LinkHandle,
    sessions: &mut [OtaSession],
    image: Option<&OtaImage>,
    stop: impl std::future::Future<Output = ()>,
    mut report: impl FnMut(FleetReport<'_>),
) -> Result<Vec<Outcome>, update::Failure> {
    tokio::pin!(stop);
    let started = tokio::time::Instant::now();
    let mut cache = image.map(|image| Sender::new(&image.bytes, image.sha256, &image.sha256));
    let mut cache_failed = None;
    let mut ticker = tokio::time::interval(OTA_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut phases: Vec<_> = sessions.iter().map(|session| session.progress().phase).collect();
    for (session, phase) in sessions.iter().zip(&phases) {
        tracing::debug!(node = %mac::full(&session.node()), phase = ?phase, "ota phase");
    }
    let mut stopped = false;
    while sessions.iter().any(|session| session.outcome().is_none()) {
        let mut sends = Vec::new();
        tokio::select! {
            () = &mut stop, if !stopped => {
                stopped = true;
                cache = None;
                let now = now();
                for session in sessions.iter_mut() {
                    sends.extend(session.cancel(now));
                }
            }
            _ = ticker.tick() => feed(sessions, OtaEvent::Tick, &mut sends),
            event = link.recv() => match event {
                Some(LinkEvent::Message(BridgeToHost::Delivered { id })) => {
                    tracing::debug!(id, status = "delivered", "ota send result");
                    feed(sessions, OtaEvent::Delivery { id, delivered: true }, &mut sends);
                }
                Some(LinkEvent::Message(BridgeToHost::Undelivered { id, reason })) => {
                    tracing::debug!(id, status = ?reason, "ota send result");
                    feed(sessions, OtaEvent::Delivery { id, delivered: false }, &mut sends);
                }
                Some(LinkEvent::Message(BridgeToHost::FromNode { slot, kind, body, .. })) => {
                    if kind == UpType::UpdateStatus.as_u8()
                        && tracing::enabled!(tracing::Level::DEBUG)
                        && let Ok(status) = UpdateStatus::decode(&body)
                    {
                        tracing::debug!(
                            %slot,
                            session = status.session,
                            state = ?status.state,
                            next_offset = status.next_offset,
                            "ota status"
                        );
                    }
                    feed(sessions, OtaEvent::FromNode { slot, kind, body: &body }, &mut sends);
                }
                Some(LinkEvent::Message(BridgeToHost::CacheImage(bytes))) => {
                    if let Some(sender) = &mut cache
                        && let Some(Op::Status(status)) = Op::decode(&bytes)
                    {
                        sender.on_status(&status);
                    }
                }
                Some(_) => {}
                None => {
                    let closed = Outcome::Failed(Failure::LinkClosed);
                    return Ok(sessions
                        .iter()
                        .map(|session| session.outcome().cloned().unwrap_or_else(|| closed.clone()))
                        .collect());
                }
            },
        }
        // Never polled once verified: it would go on to `Install`, which the cache refuses.
        if let Some(sender) = &cache {
            if sender.is_verified() {
                cache = None;
                let elapsed = started.elapsed();
                tracing::debug!(elapsed_ms = elapsed.as_millis(), "ota cache verified");
                report(FleetReport::Cached(elapsed));
                feed(sessions, OtaEvent::Cached, &mut sends);
            } else if let Stage::Failed(failure) = sender.stage() {
                cache = None;
                tracing::debug!(failure = ?failure, "ota cache failed");
                cache_failed = Some(failure);
                let now = now();
                for session in sessions.iter_mut() {
                    sends.extend(session.cancel(now));
                }
            }
        }
        // Polled after every event, so the next op goes as soon as a status allows it.
        if let Some(sender) = &mut cache {
            let now = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let mut buf = [0u8; OP_MAX];
            while let Some(len) = sender.next(now, &mut buf) {
                let op = UpdateOp::from_slice(&buf[..len]).expect("an op fits OP_MAX");
                sends.push(HostToBridge::CacheImage(op));
            }
        }
        for cmd in sends {
            log_send(&cmd);
            if let Err(e) = link.send_urgent(cmd) {
                // The session's own timeouts resend it, and the upload's.
                tracing::warn!("dropping an update record: {e}");
            }
        }
        for (i, (session, phase)) in sessions.iter().zip(&mut phases).enumerate() {
            let now_progress = session.progress();
            if now_progress.phase != *phase {
                *phase = now_progress.phase;
                tracing::debug!(
                    node = %mac::full(&session.node()),
                    phase = ?phase,
                    acked = now_progress.acked,
                    "ota phase"
                );
                if let Some(outcome) = session.outcome() {
                    let node = session.node();
                    tracing::debug!(node = %mac::full(&node), outcome = ?outcome, "ota outcome");
                }
            }
            report(FleetReport::Session(i, &now_progress));
        }
    }
    if let Some(failure) = cache_failed {
        return Err(failure);
    }
    Ok(sessions.iter().filter_map(|session| session.outcome().cloned()).collect())
}

/// Hand `event` to every session still running, gathering what they send.
fn feed(sessions: &mut [OtaSession], event: OtaEvent<'_>, sends: &mut Vec<HostToBridge>) {
    let now = now();
    for session in sessions.iter_mut().filter(|session| session.outcome().is_none()) {
        sends.extend(session.handle(now, event));
    }
}

/// Log one update record at debug level.
fn log_send(cmd: &HostToBridge) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    match cmd {
        HostToBridge::ToNode { slot, id, kind, body } => {
            let kind = match DownMsg::decode(*kind, body) {
                Ok(DownMsg::UpdateBegin(_)) => "begin",
                Ok(DownMsg::UpdateCommit(_)) => "commit",
                Ok(DownMsg::UpdateAbort(_)) => "abort",
                Ok(DownMsg::UpdateConfirm(_)) => "confirm",
                _ => "other",
            };
            tracing::debug!(%slot, id, kind, len = body.len(), "ota send");
        }
        HostToBridge::ToNodeChunk { slot, id, offset, len, .. } => {
            tracing::debug!(%slot, id, kind = "chunk", offset, len, "ota send");
        }
        _ => {}
    }
}

/// Applied settings as `capture_settings` rows: applied rather than requested, because no-ops
/// belong to the engine.
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
    use std::time::Instant;

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
        assert!(!host.suspended(), "a bridge whose receiver says nothing");
        bridge.feed_bytes(GGA_NO_FIX, 1_000);
        bridge.feed_bytes(GSV, 1_000);
        stand_host_down(Some(&host), Some(&bridge), false, 1_000);
        assert!(!host.suspended(), "sentences with no bridge connected");
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

        engine.handle(Event::Command(Command::SetPool { pool: ChannelPool::Us }), clock);
        log.record(&store, engine.capture_settings(), clock);
        // A repeated request is a no-op.
        engine.handle(Event::Command(Command::SetPool { pool: ChannelPool::Us }), clock);
        log.record(&store, engine.capture_settings(), clock);
        assert_eq!(log.next_sequence, 2);

        let reversed = Now { unix_ms: 999, ..clock };
        engine.handle(Event::Command(Command::SetPool { pool: ChannelPool::Eu }), reversed);
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
        assert!(values[0].contains("pool=all;"));
        assert!(values[1].contains("at_ms=1000;pool=us;"));
        assert!(values[2].contains("at_ms=999;pool=eu;"));
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
        let second = CaptureSettings { record_raw: !first.record_raw, ..first };
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
