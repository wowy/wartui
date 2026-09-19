//! A GPS on the host, read as NMEA over a serial port.
//!
//! The receiver runs on its own thread and publishes the last fix it saw into a
//! shared cell. Nothing above it blocks on a satellite: [`Gps::latest`] is a
//! mutex and a copy, so [`crate::PositionChain::resolve`] stays the cheap
//! synchronous call the engine needs it to be.
//!
//! **The reader stamps arrival time itself**, being the only thing that knows when a
//! byte turned up. Whether that age is too much is [`crate::PositionChain`]'s.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::nmea::{Nmea, NmeaError, Report};
use crate::position::{Fix, PositionSource};

/// Longest line worth assembling. NMEA 0183 caps a sentence at 82 characters;
/// this leaves room for the proprietary sentences that ignore that, and is
/// still short enough that a receiver spewing binary cannot grow the buffer.
const MAX_LINE: usize = 256;

/// How long to block in a read before looking at the stop flag again.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// First retry delay after the port fails, doubling to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_millis(250);

/// Slowest retry: an unplugged puck is worth checking for every few seconds.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How long a port has to stay readable before the backoff is forgiven.
///
/// Opening is not the same as working: a device node left behind after an unplug
/// opens and then fails on the first read, and resetting on the open alone would
/// retry that four times a second for the rest of the capture.
const HEALTHY: Duration = Duration::from_secs(2);

/// Which port to read, and how fast.
#[derive(Debug, Clone)]
pub struct GpsConfig {
    /// Device path, e.g. `/dev/cu.usbserial-1420`.
    pub port: String,
    /// Line rate. Most receivers ship at 9600; u-blox modules often at 38400.
    pub baud: u32,
}

/// What the receiver is doing, in terms fit to put on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpsStatus {
    /// The port has not been opened yet.
    Connecting,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpsView {
    /// What it is doing.
    pub status: GpsStatus,
    /// What it has done.
    pub counters: GpsCounters,
    /// Unix milliseconds the last fix arrived, if one ever has.
    pub last_fix_ms: Option<i64>,
}

#[derive(Debug)]
struct Inner {
    nmea: Nmea,
    fix: Option<Fix>,
    received_at_ms: Option<i64>,
    status: GpsStatus,
    counters: GpsCounters,
}

/// A handle on the receiver, cheap to clone and safe to share.
#[derive(Debug, Clone)]
pub struct Gps {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
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
                status: GpsStatus::Connecting,
                counters: GpsCounters::default(),
            })),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open `config.port` on a background thread and keep it open, reconnecting
    /// on its own if the receiver is unplugged and put back.
    ///
    /// Returns immediately: observations with no position are still worth having.
    #[must_use]
    pub fn spawn(config: GpsConfig) -> Self {
        let gps = Self::detached();
        let worker = gps.clone();
        let started = std::thread::Builder::new()
            .name("wartui-gps".to_owned())
            .spawn(move || worker.read_forever(&config));
        if let Err(e) = started {
            gps.set_status(GpsStatus::Failed(format!("starting the reader thread: {e}")));
        }
        gps
    }

    /// Ask the reader thread to finish. Idempotent, and harmless on a detached
    /// receiver.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// The last fix and the Unix milliseconds it arrived.
    #[must_use]
    pub fn latest(&self) -> Option<(Fix, i64)> {
        let inner = self.lock();
        Some((inner.fix?, inner.received_at_ms?))
    }

    /// What to say about the receiver on screen.
    #[must_use]
    pub fn view(&self) -> GpsView {
        let inner = self.lock();
        GpsView {
            status: inner.status.clone(),
            counters: inner.counters,
            last_fix_ms: inner.received_at_ms,
        }
    }

    /// Offer one line to the parser, as of `now_ms`.
    ///
    /// Public because it is the seam the reader thread and the tests share.
    pub fn feed(&self, line: &[u8], now_ms: i64) {
        let mut inner = self.lock();
        match inner.nmea.parse(line) {
            Ok(Report::Fix(new)) => {
                inner.counters.sentences += 1;
                inner.counters.fixes += 1;
                // A cycle is two sentences describing the same instant, and only
                // GGA carries altitude, satellite count and dilution. So the
                // position is replaced and the rest carried: what a sentence does
                // not mention, it is not contradicting.
                let previous = inner.fix;
                let satellites = new.satellites.or(match inner.status {
                    GpsStatus::Fixed { satellites } => satellites,
                    _ => None,
                });
                inner.status = GpsStatus::Fixed { satellites };
                inner.fix = Some(Fix {
                    lat: Some(new.lat),
                    lon: Some(new.lon),
                    alt: new.alt.or_else(|| previous.and_then(|fix| fix.alt)),
                    accuracy: new.accuracy.or_else(|| previous.and_then(|fix| fix.accuracy)),
                    source: PositionSource::Gps,
                    at_ms: new.at_ms,
                });
                inner.received_at_ms = Some(now_ms);
            }
            Ok(Report::NoFix) => {
                inner.counters.sentences += 1;
                // Deliberately leaves the last fix in place: a receiver that loses
                // lock under a bridge has not moved the host, and the chain's
                // staleness rule decides when to stop believing it.
                if !matches!(inner.status, GpsStatus::Fixed { .. }) {
                    inner.status = GpsStatus::Searching;
                }
            }
            Ok(Report::Other) => inner.counters.sentences += 1,
            Err(NmeaError::NotASentence | NmeaError::Checksum | NmeaError::Malformed) => {
                inner.counters.rejected += 1;
            }
        }
    }

    fn set_status(&self, status: GpsStatus) {
        // A reader thread nobody is watching is the most invisible thing in the
        // program: the rows keep being written, they just stop saying where.
        match &status {
            GpsStatus::Failed(reason) => tracing::warn!(reason = %reason, "gps unreadable"),
            other => tracing::info!(status = ?other, "gps"),
        }
        self.lock().status = status;
    }

    /// A poisoned mutex means a previous holder panicked mid-update; what it left is
    /// still readable and better than taking the capture down over a position.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_forever(&self, config: &GpsConfig) {
        let mut backoff = MIN_BACKOFF;
        while !self.stop.load(Ordering::Relaxed) {
            let opening = serialport::new(&config.port, config.baud)
                .timeout(READ_TIMEOUT)
                // As on the bridge's port: the one setting that keeps the driver
                // from moving RTS by itself.
                .flow_control(serialport::FlowControl::None)
                .open();
            match opening {
                Ok(port) => {
                    if !matches!(self.view().status, GpsStatus::Fixed { .. }) {
                        self.set_status(GpsStatus::Searching);
                    }
                    let opened = Instant::now();
                    let reason = self.read_port(port);
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if opened.elapsed() >= HEALTHY {
                        backoff = MIN_BACKOFF;
                    }
                    self.set_status(GpsStatus::Failed(reason));
                }
                Err(e) => self.set_status(GpsStatus::Failed(format!("{}: {e}", config.port))),
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Read until the port fails, returning why in terms fit to show a user.
    fn read_port(&self, mut port: Box<dyn serialport::SerialPort>) -> String {
        use std::io::{ErrorKind, Read};

        let mut lines = Lines::default();
        let mut buf = [0u8; 512];
        while !self.stop.load(Ordering::Relaxed) {
            match port.read(&mut buf) {
                Ok(0) => return "the receiver closed the port".to_owned(),
                Ok(n) => {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    lines.push(&buf[..n], |line| self.feed(line, now_ms));
                }
                // The read timeout exists to get back here and check the stop
                // flag; a silent receiver is not a broken one.
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return e.to_string(),
            }
        }
        "stopped".to_owned()
    }
}

/// Reassembles lines from however the OS chose to split the stream.
#[derive(Debug, Default)]
struct Lines {
    buf: Vec<u8>,
    /// Set when a line grew past [`MAX_LINE`]: everything up to the next
    /// newline is discarded rather than kept and mis-parsed.
    overrun: bool,
}

impl Lines {
    fn push(&mut self, bytes: &[u8], mut yield_line: impl FnMut(&[u8])) {
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

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76";

    #[test]
    fn a_receiver_that_has_never_had_a_fix_offers_nothing() {
        let gps = Gps::detached();
        assert_eq!(gps.latest(), None);
        gps.feed(GGA_NO_FIX, 1_000);
        assert_eq!(gps.latest(), None);
        assert_eq!(gps.view().status, GpsStatus::Searching);
    }

    #[test]
    fn a_fix_is_kept_with_the_time_it_arrived_not_the_time_it_was_taken() {
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
    fn the_altitude_and_accuracy_survive_the_sentence_that_does_not_carry_them() {
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
    fn losing_lock_does_not_throw_away_the_last_known_position() {
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
    fn garbage_is_counted_rather_than_mistaken_for_a_place() {
        let gps = Gps::detached();
        gps.feed(b"\xff\xfe binary junk", 1_000);
        gps.feed(GGA, 2_000);
        let view = gps.view();
        assert_eq!(view.counters.rejected, 1);
        assert_eq!(view.counters.fixes, 1);
        assert_eq!(view.last_fix_ms, Some(2_000));
    }

    #[test]
    fn sentences_split_across_reads_are_reassembled() {
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
    fn a_receiver_talking_something_other_than_nmea_cannot_grow_the_buffer() {
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
