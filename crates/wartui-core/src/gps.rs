//! A GPS receiver's state, fed NMEA: a puck on a host serial port, or the bridge's own receiver.
//!
//! [`Gps`] holds the last fix, the sky and the diagnostics, and parses whatever it is fed. Nothing
//! above blocks on a satellite: [`Gps::latest`] is a mutex and a copy, so
//! [`crate::PositionChain::resolve`] stays cheap and synchronous for the engine.
//!
//! **It has no thread of its own; two kinds of reader feed it.** The bridge's receiver has no
//! thread at all: its bytes arrive on the link, and the runtime hands them to
//! [`Gps::feed_bytes`], which splits UBX from NMEA ([`crate::ubx::Demux`]) and feeds the same
//! parser a puck does. A host port's reader lives outside the crate (`wartui-desktop`'s `gps`),
//! since only a desktop reads a USB receiver: it builds its receiver with [`Gps::for_reader`] and
//! drives it through [`Gps::feed`], [`Gps::tick`], [`Gps::set_status`], [`Gps::settle`],
//! [`Gps::unsettle`] and [`Gps::hold_no_port`], polling [`Gps::is_stopped`] and
//! [`Gps::is_suspended`]. This crate names no serial port.
//!
//! **A host port's reader stands down while the connected bridge's receiver is sending
//! sentences** ([`Gps::is_talking`], [`Gps::set_suspended`], which
//! [`crate::PositionChain::stand_down`] calls for a tier standing down
//! [`crate::position::StandDown::WhileAboveTalks`]), so one capture reads one receiver. The rule
//! keys on sentences rather than on a fix, so the view can follow the bridge's receiver acquiring
//! satellites. A port the operator named with `--gps` never stands down: reading it is their
//! instruction.
//!
//! The bridge's stream restarts with each connection ([`Gps::reset_stream`]): a sentence cut off by
//! a disconnect must not join the first bytes after the reconnect. Only a sentence from the new
//! stream counts toward standing down, so a reconnect does not stand the puck down on what the
//! bridge said before it. The retained fix still serves the chain until it ages out.
//!
//! **Arrival time is stamped as each line is fed**, by the reader, the only thing that knows when
//! a byte turned up. [`crate::PositionChain`] decides whether that age is too much.
//! Diagnostics use the same age limit, including on read timeouts, without changing the retained
//! fix or the screen status. Logging belongs here so resolving a position stays free of effects.
//!
//! The sky starts over whenever the reader settles on a port or gives one up ([`Gps::settle`],
//! [`Gps::unsettle`]), so a lost or replaced receiver shows none of the old one's satellites. The
//! retained fix is not reset: the chain ages it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::nmea::{Nmea, NmeaError, Report, Sky};
use crate::position::{DEFAULT_MAX_AGE, Fix, PositionSource, is_fresh};
use crate::ubx::{Demux, ValsetReply};

/// Longest line worth assembling. NMEA 0183 caps a sentence at 82 characters. This leaves room for
/// proprietary sentences that ignore that, yet stops a receiver spewing binary from growing the
/// buffer.
const MAX_LINE: usize = 256;

/// The longest gap between fix reports that still counts as holding a fix ([`Gps::fix_run`]).
/// A report after a longer gap starts a new run.
/// At 1 Hz this rides out one late report; a receiver that has gone quiet past it holds nothing.
pub const FIX_GAP: Duration = Duration::from_millis(1_500);

/// What the receiver is doing, in terms fit to put on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpsStatus {
    /// The port has not been opened yet.
    Connecting,
    /// Listening to a port at a rate, to find out whether it is a receiver.
    Scanning {
        /// The port being listened to.
        port: String,
        /// The rate being tried.
        baud: u32,
    },
    /// Every port was listened to and none was a receiver. Distinct from [`GpsStatus::Failed`]
    /// because it is not a fault and raises no fault line (`wartui-desktop`'s reader says why).
    NoReceiver,
    /// Sentences are arriving and the receiver says it has no fix. Normal for
    /// the first half-minute after a cold start, and for indoors forever.
    Searching,
    /// A position arrived.
    Fixed {
        /// Satellites in the solution, when the receiver said.
        satellites: Option<u8>,
    },
    /// The port could not be opened or stopped reading. The thread keeps
    /// retrying; this is what it will say in the meantime.
    Failed(String),
}

/// Totals worth showing, so a receiver talking a dialect this parser does not
/// understand looks different from one that is not talking at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpsCounters {
    /// Sentences that parsed, whether or not they carried a position.
    pub sentences: u64,
    /// Positions taken from them.
    pub fixes: u64,
    /// Lines that failed a checksum or a field. One or two at start-up is the reader
    /// joining a stream mid-sentence; a steady flow is the wrong baud rate.
    pub rejected: u64,
}

/// The receiver's state as of a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct GpsView {
    /// What it is doing.
    pub status: GpsStatus,
    /// What it has done.
    pub counters: GpsCounters,
    /// Unix milliseconds the last fix arrived, if one ever has.
    pub last_fix_ms: Option<i64>,
    /// The port and rate being read, once the search has settled on one.
    pub settled: Option<(String, u32)>,
    /// Whether the operator chose the rate rather than the ladder. Only then is `--gps-baud` worth
    /// mentioning: a rate the ladder chose already produced valid sentences, so later unreadable
    /// lines mean something else.
    pub pinned_baud: bool,
    /// What the receiver says about its fix and its sky, for the view's GPS modal.
    pub sky: Sky,
    /// Unix milliseconds the last sentence parsed, if one ever has. The sky is as of then.
    pub heard_ms: Option<i64>,
    /// How long the receiver may go quiet before its sentences count as stale (`--gps-max-age`).
    pub max_age: Duration,
    /// Whether this is the bridge's receiver, read over the link rather than a host port.
    pub via_bridge: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Diagnostic {
    Status(GpsStatus),
    Fresh,
    Stale,
}

#[derive(Debug)]
struct Inner {
    nmea: Nmea,
    fix: Option<Fix>,
    received_at_ms: Option<i64>,
    /// When the current unbroken run of fix reports began, cleared by a report of no fix.
    fixed_since_ms: Option<i64>,
    heard_ms: Option<i64>,
    status: GpsStatus,
    counters: GpsCounters,
    settled: Option<(String, u32)>,
    pinned_baud: bool,
    diagnostic: Option<Diagnostic>,
    last_failure: Option<String>,
    max_age: Duration,
    /// The bridge's stream, split and reassembled. Unused by the puck reader, which keeps its own
    /// [`Lines`] on its thread.
    demux: Demux,
    lines: Lines,
    /// Whether a sentence has parsed since the stream last restarted ([`Gps::reset_stream`]).
    heard_in_stream: bool,
    /// The receiver's last answer to its rate, so a repeated answer is logged once.
    valset: Option<ValsetReply>,
    /// Whether the receiver has said its transmit buffer is full, so that is logged once.
    txbuf_full: bool,
}

impl Inner {
    fn set_status(&mut self, status: GpsStatus) {
        self.status = status.clone();
        if matches!(status, GpsStatus::Scanning { .. }) {
            tracing::debug!(status = ?status, "gps scanning");
            return;
        }
        // Reopening a port is not recovery: it can fail its first read on every retry.
        if status == GpsStatus::Searching && self.last_failure.is_some() {
            return;
        }
        self.note_status(status);
    }

    fn note_status(&mut self, status: GpsStatus) {
        let diagnostic = Diagnostic::Status(status.clone());
        if self.diagnostic.as_ref() == Some(&diagnostic) {
            return;
        }
        match &status {
            GpsStatus::Failed(reason) => {
                if self.last_failure.as_ref() != Some(reason) {
                    tracing::warn!(reason = %reason, "gps unreadable");
                    self.last_failure = Some(reason.clone());
                }
            }
            other => tracing::info!(status = ?other, "gps"),
        }
        self.diagnostic = Some(diagnostic);
    }

    fn tick(&mut self, now_ms: i64) {
        if self.diagnostic == Some(Diagnostic::Fresh)
            && let Some(at_ms) = self.received_at_ms
            && !is_fresh(at_ms, now_ms, self.max_age)
        {
            tracing::warn!(last_fix_ms = at_ms, max_age_ms = ?self.max_age.as_millis(), "gps fix stale");
            self.diagnostic = Some(Diagnostic::Stale);
        }
    }

    /// Drop what the last receiver said about its sky, so a lost or replaced one shows no
    /// satellites it is not reporting. The retained fix and status belong to the position chain.
    fn forget_receiver(&mut self) {
        // The host's choice for this receiver, not something the old one said.
        let sky_lists = self.nmea.sky_lists();
        self.nmea = Nmea::new();
        self.nmea.set_sky_lists(sky_lists);
        self.heard_ms = None;
    }

    fn note_fix(&mut self) {
        self.last_failure = None;
        if self.diagnostic != Some(Diagnostic::Fresh) {
            if self.received_at_ms.is_none() {
                tracing::info!("gps fix acquired");
            } else {
                tracing::info!("gps fix recovered");
            }
            self.diagnostic = Some(Diagnostic::Fresh);
        }
    }

    /// The host writes the rate with every bridge status poll, so the receiver answers every few
    /// seconds. Only a change is worth a line. Takes the field rather than `self`, so the demux's
    /// packet callback can hold it while the line callback holds the parser.
    fn note_valset(seen: &mut Option<ValsetReply>, reply: ValsetReply) {
        if *seen == Some(reply) {
            return;
        }
        match reply {
            ValsetReply::Accepted => tracing::debug!("the GPS receiver took its rate"),
            ValsetReply::Refused => tracing::warn!("the GPS receiver refused its rate"),
        }
        *seen = Some(reply);
    }

    /// A `TXT` sentence saying `txbuf` is how a u-blox receiver reports dropping its own output.
    fn note_txt(&mut self, line: &[u8]) {
        let txt = line.get(3..6) == Some(b"TXT");
        if txt && !self.txbuf_full && line.windows(5).any(|w| w == b"txbuf") {
            tracing::warn!("the GPS receiver is dropping output: its transmit buffer is full");
            self.txbuf_full = true;
        }
    }

    /// Offer one line to the parser, as of `now_ms`.
    fn feed(&mut self, line: &[u8], now_ms: i64) {
        self.tick(now_ms);
        self.feed_line(line, now_ms);
    }

    /// [`Self::feed`] without the tick, for a batch of lines that has already ticked once.
    fn feed_line(&mut self, line: &[u8], now_ms: i64) {
        let parsed = self.nmea.parse(line);
        if parsed.is_ok() {
            self.heard_ms = Some(now_ms);
            self.heard_in_stream = true;
        }
        match parsed {
            Ok(Report::Fix(new)) => {
                self.note_fix();
                self.counters.sentences += 1;
                self.counters.fixes += 1;
                // A cycle is two sentences about one instant, and only GGA carries altitude,
                // satellites and dilution. So the position is replaced and the rest carried: what a
                // sentence omits, it does not contradict.
                let previous = self.fix;
                let satellites = new.satellites.or(match self.status {
                    GpsStatus::Fixed { satellites } => satellites,
                    _ => None,
                });
                self.status = GpsStatus::Fixed { satellites };
                self.fix = Some(Fix {
                    lat: Some(new.lat),
                    lon: Some(new.lon),
                    alt: new.alt.or_else(|| previous.and_then(|fix| fix.alt)),
                    accuracy: new.accuracy.or_else(|| previous.and_then(|fix| fix.accuracy)),
                    source: PositionSource::Gps,
                    at_ms: new.at_ms,
                });
                // A report after a silence past the gap starts a new run.
                if self.received_at_ms.is_none_or(|at_ms| !is_fresh(at_ms, now_ms, FIX_GAP)) {
                    self.fixed_since_ms = None;
                }
                self.received_at_ms = Some(now_ms);
                self.fixed_since_ms.get_or_insert(now_ms);
            }
            Ok(Report::NoFix) => {
                self.last_failure = None;
                self.fixed_since_ms = None;
                self.counters.sentences += 1;
                // Keep the last fix: losing lock under a bridge has not moved the host, and the
                // chain's staleness rule decides when to stop believing it.
                if !matches!(self.status, GpsStatus::Fixed { .. }) {
                    self.status = GpsStatus::Searching;
                    self.note_status(GpsStatus::Searching);
                }
            }
            Ok(Report::Other) => {
                self.counters.sentences += 1;
                self.note_txt(line);
            }
            Err(NmeaError::NotASentence | NmeaError::Checksum | NmeaError::Malformed) => {
                self.counters.rejected += 1;
            }
        }
    }
}

/// A handle on the receiver, cheap to clone and safe to share.
#[derive(Debug, Clone)]
pub struct Gps {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    /// Set while the connected bridge's receiver is sending sentences ([`Gps::is_talking`]): the
    /// puck reader holds no port and searches for none.
    suspended: Arc<AtomicBool>,
    /// Whether the operator named the port, which [`Gps::set_suspended`] then leaves alone.
    pinned: bool,
    /// Whether this is the bridge's receiver ([`Gps::bridged`]). A field rather than in `inner`,
    /// so the chain finds the bridge's receiver without taking the reader thread's lock.
    bridged: bool,
}

impl Gps {
    /// A receiver with nothing behind it: it holds state and accepts [`Gps::feed`]
    /// with no thread reading a port, which is what makes the chain testable.
    #[must_use]
    pub fn detached() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                nmea: Nmea::new(),
                fix: None,
                received_at_ms: None,
                fixed_since_ms: None,
                heard_ms: None,
                status: GpsStatus::Connecting,
                counters: GpsCounters::default(),
                settled: None,
                pinned_baud: false,
                diagnostic: None,
                last_failure: None,
                max_age: DEFAULT_MAX_AGE,
                demux: Demux::new(),
                lines: Lines::default(),
                heard_in_stream: false,
                valset: None,
                txbuf_full: false,
            })),
            stop: Arc::new(AtomicBool::new(false)),
            suspended: Arc::new(AtomicBool::new(false)),
            pinned: false,
            bridged: false,
        }
    }

    /// The bridge's receiver: detached, and fed by the runtime through [`Gps::feed_bytes`].
    /// `max_age` is the staleness limit its diagnostics and [`Gps::is_talking`] use, as
    /// [`Gps::for_reader`]'s is for a puck. Its sky lists start off ([`Gps::set_sky_lists`]), as
    /// the host's configuration of the receiver does.
    #[must_use]
    pub fn bridged(max_age: Duration) -> Self {
        let gps = Self { bridged: true, ..Self::detached() };
        {
            let mut inner = gps.lock();
            inner.max_age = max_age;
            inner.nmea.set_sky_lists(false);
        }
        gps
    }

    /// The receiver a host port's reader feeds: detached, for a reader outside this crate to drive.
    /// `pinned_port` and `pinned_baud` say whether the operator named the port and the rate, and
    /// `max_age` is how old a fix may be before its diagnostics call it stale.
    #[must_use]
    pub fn for_reader(pinned_port: bool, pinned_baud: bool, max_age: Duration) -> Self {
        let gps = Self { pinned: pinned_port, ..Self::detached() };
        {
            let mut inner = gps.lock();
            inner.pinned_baud = pinned_baud;
            inner.max_age = max_age;
        }
        gps
    }

    /// Ask the reader to finish. Idempotent, and harmless when detached.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether [`Self::stop`] has been called. A host port's reader polls it between reads.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Stand the puck reader down, or let it search again. While suspended a host port's reader
    /// lets go of its port, opens none, and forgets the sky. Cheap and idempotent, so the
    /// runtime calls it on every tick; only a change is logged. A no-op for a port the operator
    /// named with `--gps`, and harmless when detached.
    pub fn set_suspended(&self, suspended: bool) {
        if self.pinned || self.suspended.swap(suspended, Ordering::Relaxed) == suspended {
            return;
        }
        if suspended {
            tracing::info!("gps search stands down: the bridge's receiver is sending sentences");
        } else {
            tracing::info!("gps search resumes");
        }
    }

    /// Whether a checksum-valid sentence arrived within the staleness limit of `now_ms`, and since
    /// the stream last restarted ([`Self::reset_stream`]). A sentence without a fix counts.
    #[must_use]
    pub fn is_talking(&self, now_ms: i64) -> bool {
        let inner = self.lock();
        inner.heard_in_stream
            && inner.heard_ms.is_some_and(|at_ms| is_fresh(at_ms, now_ms, inner.max_age))
    }

    /// Whether the operator named this receiver's port with `--gps`.
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether this is the bridge's receiver ([`Self::bridged`]), fed from the link.
    #[must_use]
    pub const fn is_bridged(&self) -> bool {
        self.bridged
    }

    /// [`Self::detached`], as if the operator had named its port.
    #[cfg(test)]
    pub(crate) fn detached_pinned() -> Self {
        Self { pinned: true, ..Self::detached() }
    }

    /// Start the bridge's stream over: drop a partial line or UBX packet, so bytes from before a
    /// reconnect never join the next stream, and require a new sentence before
    /// [`Self::is_talking`] holds. The retained fix and the sky stay, as the chain and the view
    /// age them.
    pub fn reset_stream(&self) {
        let mut inner = self.lock();
        inner.demux = Demux::new();
        inner.lines = Lines::default();
        inner.heard_in_stream = false;
    }

    /// Let `GSA` and `GSV` build the satellites and fix mode when `on`, as the host turns those
    /// sentences on or off ([`Nmea::set_sky_lists`]). Either way the lists so far are dropped,
    /// and the fix and the `GGA` fields stay. Kept across a lost receiver.
    pub fn set_sky_lists(&self, on: bool) {
        self.lock().nmea.set_sky_lists(on);
    }

    /// The last fix and the Unix milliseconds it arrived.
    #[must_use]
    pub fn latest(&self) -> Option<(Fix, i64)> {
        let inner = self.lock();
        Some((inner.fix?, inner.received_at_ms?))
    }

    /// The run of fix reports the receiver holds as of `now_ms`, named by the arrival stamp of
    /// its first report: the reports have run unbroken by a report of no fix, and the latest
    /// arrived within [`FIX_GAP`]. `None` while it holds none. The retained fix [`Self::latest`]
    /// serves outlives this: one fix followed by silence holds nothing.
    ///
    /// The stamp is the run's identity, not a time to measure from: it is wall-clock, which an
    /// NTP step moves. A caller times the run on its own monotonic clock.
    #[must_use]
    pub fn fix_run(&self, now_ms: i64) -> Option<i64> {
        let inner = self.lock();
        let since = inner.fixed_since_ms?;
        is_fresh(inner.received_at_ms?, now_ms, FIX_GAP).then_some(since)
    }

    /// What to say about the receiver on screen.
    #[must_use]
    pub fn view(&self) -> GpsView {
        let inner = self.lock();
        GpsView {
            status: inner.status.clone(),
            counters: inner.counters,
            last_fix_ms: inner.received_at_ms,
            settled: inner.settled.clone(),
            pinned_baud: inner.pinned_baud,
            sky: inner.nmea.sky().clone(),
            heard_ms: inner.heard_ms,
            max_age: inner.max_age,
            via_bridge: self.bridged,
        }
    }

    /// Offer one line to the parser, as of `now_ms`. Public as the seam the reader thread and the
    /// tests share.
    pub fn feed(&self, line: &[u8], now_ms: i64) {
        self.lock().feed(line, now_ms);
    }

    /// Offer a piece of the bridge's receiver stream, as of `now_ms`, in whatever pieces the
    /// bridge read it. Complete NMEA lines reach the parser. The receiver's answer to its rate is
    /// logged, and other UBX is ignored.
    pub fn feed_bytes(&self, bytes: &[u8], now_ms: i64) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.tick(now_ms);
        // Out of `inner` for the call, so the line callback can feed the parser it holds while the
        // packet callback notes the receiver's answer.
        let mut demux = std::mem::take(&mut inner.demux);
        let mut lines = std::mem::take(&mut inner.lines);
        let mut valset = inner.valset.take();
        demux.push(
            bytes,
            |run| lines.push(run, |line| inner.feed_line(line, now_ms)),
            |packet| {
                if let Some(reply) = packet.valset_reply() {
                    Inner::note_valset(&mut valset, reply);
                }
            },
        );
        inner.demux = demux;
        inner.lines = lines;
        inner.valset = valset;
    }

    /// Age the retained fix as of `now_ms`, for the staleness log. The puck reader does this on
    /// every read. A bridge's receiver that has gone quiet sends no bytes to do it, so the runtime
    /// calls this on its tick.
    pub fn tick(&self, now_ms: i64) {
        self.lock().tick(now_ms);
    }

    /// Say what a host port's reader is doing. A repeated status is logged once, and a reopened
    /// port reporting `Searching` after a failure is not taken for recovery.
    pub fn set_status(&self, status: GpsStatus) {
        self.lock().set_status(status);
    }

    /// Whether the puck reader is standing down ([`Self::set_suspended`]). A host port's reader
    /// polls it between reads.
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.suspended.load(Ordering::Relaxed)
    }

    /// Record that a host port's reader has settled on `port` at `baud`, and start the sky over:
    /// what the last receiver said is not this one's.
    pub fn settle(&self, port: &str, baud: u32) {
        let mut inner = self.lock();
        inner.settled = Some((port.to_owned(), baud));
        inner.forget_receiver();
    }

    /// Record that a host port's reader has given its port up, and start the sky over.
    pub fn unsettle(&self) {
        let mut inner = self.lock();
        inner.settled = None;
        inner.forget_receiver();
    }

    /// Record that a host port's reader holds no port and searches for none, as it stands down:
    /// the settled port and the sky are forgotten, since the receiver read on resume may be another
    /// one, and the status is [`GpsStatus::Connecting`], since standing down is not a fault.
    pub fn hold_no_port(&self) {
        let mut inner = self.lock();
        inner.settled = None;
        inner.forget_receiver();
        inner.status = GpsStatus::Connecting;
        inner.diagnostic = None;
        inner.last_failure = None;
    }

    /// A poisoned mutex means a previous holder panicked mid-update; what it left is
    /// still readable and better than taking the capture down over a position.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Reassembles lines from however the OS chose to split the stream, for whatever reads a host
/// port and for the receiver search's samples.
#[derive(Debug, Default)]
pub struct Lines {
    buf: Vec<u8>,
    /// Set when a line grew past [`MAX_LINE`]: everything up to the next
    /// newline is discarded rather than kept and mis-parsed.
    overrun: bool,
}

impl Lines {
    /// Take the next `bytes` read, and yield each whole line they complete, without its newline.
    /// A line longer than the cap is dropped whole, up to the next newline.
    pub fn push(&mut self, bytes: &[u8], mut yield_line: impl FnMut(&[u8])) {
        for byte in bytes {
            if *byte == b'\n' {
                if !self.overrun && !self.buf.is_empty() {
                    yield_line(&self.buf);
                }
                self.buf.clear();
                self.overrun = false;
            } else if self.buf.len() < MAX_LINE {
                self.buf.push(*byte);
            } else {
                self.buf.clear();
                self.overrun = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nmea::FixMode;
    use crate::position::DEFAULT_MAX_AGE;
    use crate::ubx::{ACK_VALSET, NAK_VALSET};

    thread_local! {
        /// The calling test's captured lines, while a [`Logs`] guard lives.
        static CAPTURED: std::cell::RefCell<Option<Vec<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Records events as `field=value` pairs into the current thread's capture.
    ///
    /// One global subscriber with a per-thread capture, not `with_default` per test: a scoped
    /// subscriber coming and going while other tests run lets tracing cache a callsite as disabled.
    struct LogSink;

    impl tracing::Subscriber for LogSink {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() > tracing::Level::INFO {
                return;
            }
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            CAPTURED.with_borrow_mut(|lines| lines.as_mut().map(|lines| lines.push(fields.0)));
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// The lines logged on this thread while it lives.
    struct Logs;

    impl Logs {
        fn capture() -> Self {
            static INSTALL: std::sync::Once = std::sync::Once::new();
            INSTALL.call_once(|| {
                tracing::subscriber::set_global_default(LogSink).expect("no other");
            });
            // A callsite another thread registered while the sink was being installed can have
            // cached it as disabled; recompute every callsite's interest.
            tracing::callsite::rebuild_interest_cache();
            CAPTURED.set(Some(Vec::new()));
            Self
        }

        fn entries(&self) -> Vec<String> {
            CAPTURED.with_borrow(|lines| lines.clone().unwrap_or_default())
        }

        fn assert_messages(&self, messages: &[&str]) {
            let entries = self.entries();
            assert_eq!(entries.len(), messages.len(), "{entries:?}");
            for (entry, message) in entries.iter().zip(messages) {
                assert!(entry.contains(message), "expected {message:?} in {entry:?}");
            }
        }
    }

    impl Drop for Logs {
        fn drop(&mut self) {
            CAPTURED.set(None);
        }
    }

    #[test]
    fn gps_logs_first_fix_when_successful_sentence_arrives() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        let entries = logs.entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].contains("gps fix acquired"), "{entries:?}");
    }

    #[test]
    fn gps_bounds_logs_when_fixes_and_satellite_counts_repeat() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        for now_ms in 1_000..1_100 {
            gps.feed(GGA, now_ms);
            gps.feed(b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*73", now_ms);
            gps.feed(
                b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,09,0.9,545.4,M,46.9,M,,*68",
                now_ms,
            );
        }
        assert_eq!(gps.view().counters.fixes, 300);
        assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(9) });
        logs.assert_messages(&["gps fix acquired"]);
    }

    #[test]
    fn gps_logs_stale_and_recovery_when_configured_age_is_exceeded() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        gps.lock().max_age = Duration::from_secs(2);
        gps.feed(GGA, 1_000);
        let latest = gps.latest();
        let status = gps.view().status;
        gps.tick(999);
        gps.tick(3_000);
        logs.assert_messages(&["gps fix acquired"]);
        gps.tick(3_001);
        gps.tick(30_000);
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
        assert_eq!(gps.latest(), latest);
        assert_eq!(gps.view().status, status);
        gps.feed(GGA, 30_001);
        gps.feed(GGA, 30_002);
        logs.assert_messages(&["gps fix acquired", "gps fix stale", "gps fix recovered"]);
    }

    #[test]
    fn gps_logs_stale_once_when_receiver_is_silent() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        assert_eq!(gps.view().max_age, Duration::from_secs(5));
        gps.feed(GGA, 1_000);
        gps.tick(6_000);
        logs.assert_messages(&["gps fix acquired"]);
        for now_ms in 6_001..6_100 {
            gps.tick(now_ms);
        }
        assert_eq!(gps.latest().unwrap().1, 1_000);
        assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(8) });
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
    }

    #[test]
    fn gps_bounds_searching_logs_when_receiver_reports_no_fix() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        for now_ms in 1_000..1_100 {
            gps.feed(GGA_NO_FIX, now_ms);
        }
        gps.tick(60_000);
        assert_eq!(gps.latest(), None);
        assert_eq!(gps.view().status, GpsStatus::Searching);
        gps.feed(GGA, 60_001);
        logs.assert_messages(&["Searching", "gps fix acquired"]);
    }

    #[test]
    fn gps_logs_stale_when_no_fix_sentences_outlast_retained_fix() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        for now_ms in [2_000, 6_000] {
            gps.feed(GGA_NO_FIX, now_ms);
        }
        logs.assert_messages(&["gps fix acquired"]);
        for now_ms in 6_001..6_100 {
            gps.feed(GGA_NO_FIX, now_ms);
        }
        assert_eq!(gps.latest().unwrap().1, 1_000);
        assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(8) });
        gps.feed(GGA, 6_100);
        logs.assert_messages(&["gps fix acquired", "gps fix stale", "gps fix recovered"]);
    }

    #[test]
    fn gps_logs_gap_when_fix_returns_without_intervening_tick() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(GGA, 6_001);
        logs.assert_messages(&["gps fix acquired", "gps fix stale", "gps fix recovered"]);
    }

    #[test]
    fn gps_bounds_failure_logs_when_failed_ports_are_retried_or_removed() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        let latest = gps.latest();
        for _ in 0..100 {
            gps.set_status(GpsStatus::Failed("read failed".to_owned()));
            gps.set_status(GpsStatus::Searching);
        }
        let gone = GpsStatus::Failed("receiver is no longer attached".to_owned());
        for _ in 0..100 {
            gps.set_status(gone.clone());
            gps.tick(60_000);
        }
        assert_eq!(gps.latest(), latest);
        assert_eq!(gps.view().status, gone);
        gps.feed(GGA, 60_001);
        gps.set_status(GpsStatus::Failed("read failed".to_owned()));
        logs.assert_messages(&[
            "gps fix acquired",
            "read failed",
            "receiver is no longer attached",
            "gps fix recovered",
            "read failed",
        ]);
    }

    #[test]
    fn gps_bounds_failure_logs_when_discovery_retries_the_same_missing_receiver() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        for _ in 0..100 {
            gps.set_status(GpsStatus::Scanning { port: "missing".to_owned(), baud: 9_600 });
            gps.set_status(GpsStatus::Failed("missing receiver".to_owned()));
        }
        gps.set_status(GpsStatus::Searching);
        gps.feed(GGA_NO_FIX, 1_000);
        gps.set_status(GpsStatus::Failed("missing receiver".to_owned()));
        logs.assert_messages(&["missing receiver", "Searching", "missing receiver"]);
    }

    #[test]
    fn gps_bounds_no_receiver_logs_when_discovery_repeats_without_a_receiver() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        for _ in 0..100 {
            gps.set_status(GpsStatus::Scanning { port: "candidate".to_owned(), baud: 9_600 });
            gps.set_status(GpsStatus::NoReceiver);
        }
        logs.assert_messages(&["NoReceiver"]);
    }

    #[test]
    fn gps_logs_stale_when_zero_age_fix_is_followed_by_unusable_sentences() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        gps.lock().max_age = Duration::ZERO;
        gps.feed(GGA, 1_000);
        gps.tick(1_000);
        logs.assert_messages(&["gps fix acquired"]);
        gps.feed(b"not NMEA", 1_001);
        gps.feed(b"$GPGSV,1,1,00*79", 1_002);
        assert_eq!(gps.latest().unwrap().1, 1_000);
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
    }

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76";

    #[test]
    fn gps_names_fix_run_by_its_first_report_when_reports_run_unbroken() {
        let gps = Gps::detached();
        assert_eq!(gps.fix_run(1_000), None);
        for now_ms in [1_000, 2_000, 3_000] {
            gps.feed(GGA, now_ms);
        }
        assert_eq!(gps.fix_run(3_500), Some(1_000));
        // A clock stepping back keeps the run.
        assert_eq!(gps.fix_run(500), Some(1_000));
    }

    #[test]
    fn gps_holds_no_fix_run_when_reports_stop_past_the_gap() {
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        assert_eq!(gps.fix_run(2_500), Some(1_000));
        assert_eq!(gps.fix_run(2_501), None);
        // The retained fix still serves the chain.
        assert!(gps.latest().is_some());
        // A fix after the silence starts a new run.
        gps.feed(GGA, 10_000);
        assert_eq!(gps.fix_run(10_000), Some(10_000));
    }

    #[test]
    fn gps_starts_new_fix_run_when_no_fix_report_arrives() {
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(GGA_NO_FIX, 2_000);
        assert_eq!(gps.fix_run(2_000), None);
        gps.feed(GGA, 3_000);
        assert_eq!(gps.fix_run(4_000), Some(3_000));
    }

    #[test]
    fn gps_has_no_fix_when_receiver_reports_none() {
        let gps = Gps::detached();
        assert_eq!(gps.latest(), None);
        gps.feed(GGA_NO_FIX, 1_000);
        assert_eq!(gps.latest(), None);
        assert_eq!(gps.view().status, GpsStatus::Searching);
    }

    #[test]
    fn gps_stamps_fix_with_arrival_time_when_fix_parsed() {
        // The two differ, and only the arrival time can say how stale a fix
        // is: the receiver's own clock is the thing being reported on.
        let gps = Gps::detached();
        gps.feed(b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*73", 5_000);
        let (fix, at) = gps.latest().expect("a fix");
        assert_eq!(at, 5_000);
        assert_eq!(fix.source, PositionSource::Gps);
        assert_eq!(fix.at_ms, Some(1_788_611_719_000));
    }

    #[test]
    fn gps_keeps_gga_altitude_and_accuracy_when_rmc_follows() {
        // Receivers emit GGA and RMC every cycle, RMC normally last, so
        // whatever the RMC does not carry is what the fix spends its life as.
        // Only GGA has an altitude or an HDOP, and both have a column waiting
        // for them in the export.
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*73", 1_100);
        let (fix, _) = gps.latest().expect("a fix");
        assert_eq!(fix.alt, Some(545.4));
        assert_eq!(fix.accuracy, Some(4.5));
        assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(8) });
    }

    #[test]
    fn gps_keeps_last_fix_when_lock_lost() {
        // Driving under a bridge should not blank the position of every
        // observation for the next second. Age is what disqualifies a fix.
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(GGA_NO_FIX, 2_000);
        let (_, at) = gps.latest().expect("the last fix");
        assert_eq!(at, 1_000);
        assert!(matches!(gps.view().status, GpsStatus::Fixed { .. }));
    }

    #[test]
    fn gps_counts_rejected_when_line_corrupt() {
        let gps = Gps::detached();
        gps.feed(b"\xff\xfe binary junk", 1_000);
        gps.feed(GGA, 2_000);
        let view = gps.view();
        assert_eq!(view.counters.rejected, 1);
        assert_eq!(view.counters.fixes, 1);
        assert_eq!(view.last_fix_ms, Some(2_000));
    }

    #[test]
    fn gps_exposes_sky_in_view_when_sentences_fed() {
        let gps = Gps::detached();
        assert_eq!(gps.view().heard_ms, None);
        gps.feed(GGA, 1_000);
        gps.feed(b"$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69", 1_100);
        gps.feed(b"$GNGSA,A,3,10,08,,,,,,,,,,,1.5,0.9,1.2,1*36", 1_200);
        gps.feed(b"not NMEA", 1_300);
        let view = gps.view();
        assert_eq!(view.heard_ms, Some(1_200), "a rejected line is not hearing the receiver");
        assert_eq!(view.sky.used_count, Some(8));
        assert_eq!(view.sky.satellites.len(), 2);
        assert!(view.sky.satellites.iter().all(|s| s.used));
    }

    #[test]
    fn gps_forgets_sky_when_receiver_is_lost() {
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(b"$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69", 1_100);
        gps.feed(b"$GNGSA,A,3,10,08,,,,,,,,,,,1.5,0.9,1.2,1*36", 1_200);
        let latest = gps.latest();
        assert!(latest.is_some());
        gps.lock().forget_receiver();
        let view = gps.view();
        assert_eq!(view.sky, Sky::default());
        assert_eq!(view.heard_ms, None);
        assert_eq!(gps.latest(), latest);
    }

    #[test]
    fn gps_drops_sky_lists_but_keeps_fix_when_sky_lists_turned_off() {
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(SKY_GSV, 1_100);
        gps.feed(SKY_GSA, 1_200);
        let latest = gps.latest();

        gps.set_sky_lists(false);
        let view = gps.view();
        assert!(view.sky.satellites.is_empty());
        assert_eq!(view.sky.mode, None);
        assert_eq!(view.sky.used_count, Some(8));
        assert!(view.sky.lat.is_some());
        assert_eq!(view.heard_ms, Some(1_200));
        assert_eq!(gps.latest(), latest);
    }

    /// Two GPS satellites in view, and a 3D `GSA` using both.
    const SKY_GSV: &[u8] = b"$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69";
    const SKY_GSA: &[u8] = b"$GNGSA,A,3,10,08,,,,,,,,,,,1.5,0.9,1.2,1*36";

    #[test]
    fn gps_counts_sky_sentences_without_listing_them_when_bridged() {
        // Off from the start, matching the engine's default.
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        let mut stream = SKY_GSV.to_vec();
        stream.extend_from_slice(b"\r\n");
        stream.extend_from_slice(SKY_GSA);
        stream.extend_from_slice(b"\r\n");
        gps.feed_bytes(&stream, 1_000);

        let view = gps.view();
        assert!(view.sky.satellites.is_empty());
        assert_eq!(view.sky.mode, None);
        assert_eq!(view.counters.sentences, 2);
        assert_eq!(view.heard_ms, Some(1_000));
        assert!(gps.is_talking(1_000), "they still stand the host's search down");

        gps.set_sky_lists(true);
        gps.feed_bytes(&stream, 1_100);
        assert_eq!(gps.view().sky.satellites.len(), 2);
        assert_eq!(gps.view().sky.mode, Some(FixMode::ThreeD));
    }

    #[test]
    fn gps_keeps_sky_lists_off_when_receiver_is_lost() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        gps.lock().forget_receiver();
        gps.feed(SKY_GSV, 1_000);
        assert!(gps.view().sky.satellites.is_empty());

        gps.set_sky_lists(true);
        gps.lock().forget_receiver();
        gps.feed(SKY_GSV, 1_100);
        assert_eq!(gps.view().sky.satellites.len(), 2);
    }

    #[test]
    fn gps_reads_bridge_view_when_bridged() {
        let gps = Gps::bridged(Duration::from_secs(3));
        let view = gps.view();
        assert!(view.via_bridge);
        assert_eq!(view.max_age, Duration::from_secs(3));
        assert_eq!(view.status, GpsStatus::Connecting);
        assert!(!Gps::detached().view().via_bridge);
    }

    #[test]
    fn gps_fixes_from_bridge_bytes_when_sentence_spans_chunks_around_an_ack() {
        // The bridge cuts its stream wherever an I2C read ended, and the receiver answers each
        // rate write between sentences. Neither may cost the sentence or count as rejected.
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        let mut stream = GGA.to_vec();
        stream.extend_from_slice(b"\r\n");
        stream.extend_from_slice(&ACK_VALSET);
        stream.extend_from_slice(GGA_NO_FIX);
        stream.extend_from_slice(b"\r\n");
        for chunk in stream.chunks(7) {
            gps.feed_bytes(chunk, 2_000);
        }
        let view = gps.view();
        assert_eq!(view.counters.fixes, 1);
        assert_eq!(view.counters.sentences, 2);
        assert_eq!(view.counters.rejected, 0);
        assert_eq!(gps.latest().map(|(_, at)| at), Some(2_000));
    }

    #[test]
    fn gps_logs_refused_rate_once_when_receiver_repeats_its_nak() {
        let logs = Logs::capture();
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        for now_ms in 0..10 {
            gps.feed_bytes(&NAK_VALSET, now_ms);
        }
        gps.feed_bytes(&ACK_VALSET, 10);
        gps.feed_bytes(&NAK_VALSET, 11);
        assert_eq!(gps.view().counters.rejected, 0);
        logs.assert_messages(&[
            "the GPS receiver refused its rate",
            "the GPS receiver refused its rate",
        ]);
    }

    #[test]
    fn gps_logs_full_transmit_buffer_once_when_receiver_reports_txbuf() {
        let logs = Logs::capture();
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        for now_ms in 0..3 {
            gps.feed_bytes(b"$GNTXT,01,01,00,txbuf alloc*61\r\n", now_ms);
        }
        gps.feed_bytes(b"$GNTXT,01,01,02,ANTSTATUS=OK*25\r\n", 3);
        assert_eq!(gps.view().counters.rejected, 0);
        logs.assert_messages(&["the GPS receiver is dropping output"]);
    }

    #[test]
    fn gps_logs_stale_when_bridge_receiver_goes_quiet_and_runtime_ticks() {
        let logs = Logs::capture();
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        let mut sentence = GGA.to_vec();
        sentence.extend_from_slice(b"\r\n");
        gps.feed_bytes(&sentence, 1_000);
        gps.tick(7_000);
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
    }

    #[test]
    fn gps_logs_suspension_once_when_set_repeatedly() {
        let logs = Logs::capture();
        let gps = Gps::detached();
        for _ in 0..3 {
            gps.set_suspended(true);
        }
        assert!(gps.is_suspended());
        for _ in 0..3 {
            gps.set_suspended(false);
        }
        logs.assert_messages(&["stands down", "resumes"]);
    }

    /// `line` as the bridge forwards it, terminated.
    fn streamed(line: &[u8]) -> Vec<u8> {
        let mut bytes = line.to_vec();
        bytes.extend_from_slice(b"\r\n");
        bytes
    }

    #[test]
    fn gps_is_talking_when_no_fix_sentences_are_within_max_age() {
        // A receiver with no sky still sends every epoch. That is the bridge's receiver at work.
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        assert!(!gps.is_talking(1_000), "nothing heard yet");
        gps.feed_bytes(&streamed(GGA_NO_FIX), 1_000);
        assert!(gps.is_talking(6_000));
        assert!(!gps.is_talking(6_001));
        gps.feed_bytes(b"$GPGSV,1,1,00*79\r\n", 6_001);
        assert!(gps.is_talking(6_001));
    }

    #[test]
    fn gps_is_not_talking_when_bytes_hold_no_valid_sentence() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        // GGA_NO_FIX with its checksum broken.
        gps.feed_bytes(b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*00\r\n", 1_000);
        gps.feed_bytes(&ACK_VALSET, 1_000);
        assert!(!gps.is_talking(1_000));
    }

    #[test]
    fn gps_is_not_talking_when_stream_resets_until_a_new_sentence_arrives() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        gps.feed_bytes(&streamed(GGA_NO_FIX), 1_000);
        gps.reset_stream();
        // Heard within the age limit, but before the reconnect.
        assert!(!gps.is_talking(2_000));
        gps.feed_bytes(&streamed(GGA_NO_FIX), 3_000);
        assert!(gps.is_talking(3_000));
    }

    #[test]
    fn gps_drops_partial_sentence_when_stream_is_reset() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        // Half a sentence, then a reconnect, then the tail of another: joined, the two would parse
        // as nothing and count as rejected, or worse, as a sentence neither sent.
        gps.feed_bytes(&GGA[..30], 1_000);
        gps.reset_stream();
        gps.feed_bytes(&GGA[30..], 2_000);
        gps.feed_bytes(b"\r\n", 2_000);
        assert_eq!(gps.view().counters.fixes, 0);
        let mut sentence = GGA.to_vec();
        sentence.extend_from_slice(b"\r\n");
        gps.feed_bytes(&sentence, 3_000);
        assert_eq!(gps.view().counters.fixes, 1);
    }

    #[test]
    fn gps_keeps_fix_and_sky_when_stream_is_reset() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        gps.set_sky_lists(true);
        let mut stream = GGA.to_vec();
        stream.extend_from_slice(b"\r\n$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69\r\n");
        gps.feed_bytes(&stream, 1_000);
        let (latest, sky) = (gps.latest(), gps.view().sky);
        assert_eq!(sky.satellites.len(), 2);
        gps.reset_stream();
        assert_eq!(gps.latest(), latest);
        assert_eq!(gps.view().sky, sky);
    }

    #[test]
    fn lines_reassembles_sentence_when_stream_arrives_in_fragments() {
        // A USB serial read returns whatever happened to be in the buffer, so
        // this is the normal case rather than an edge one.
        let gps = Gps::detached();
        let mut lines = Lines::default();
        let stream = [&GGA[..20], &GGA[20..], b"\r\n$GPGGA"];
        for chunk in stream {
            lines.push(chunk, |line| gps.feed(line, 3_000));
        }
        assert_eq!(gps.view().counters.fixes, 1, "one whole sentence, once");
    }

    #[test]
    fn lines_caps_buffer_when_line_exceeds_max_line() {
        let gps = Gps::detached();
        let mut lines = Lines::default();
        lines.push(&vec![b'x'; MAX_LINE * 4], |line| gps.feed(line, 1_000));
        assert!(lines.buf.len() <= MAX_LINE);
        // And the sentence after the flood still parses.
        lines.push(b"\n", |line| gps.feed(line, 1_000));
        lines.push(GGA, |line| gps.feed(line, 2_000));
        lines.push(b"\n", |line| gps.feed(line, 2_000));
        assert_eq!(gps.view().counters.fixes, 1);
    }
}
