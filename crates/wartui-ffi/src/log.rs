//! The library's log lines, handed to the phone's logger.
//!
//! The crates below log through `tracing`, and a phone has a logger of its own (`Log` on Android,
//! `os_log` on iOS). So [`log_to`] sets one global subscriber, the registry with a layer that
//! hands each line to the phone's [`LogSink`]. It is set once per process, as a global subscriber
//! must be.
//!
//! **A sink never takes the library down.** A sink that fails is ignored, and a line logged on a
//! thread while that thread is inside the sink is dropped, so a sink that logs through code that
//! reaches back into the library cannot recurse.

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::Arc;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::Registry;

/// How severe a log line is, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    const fn of(level: Level) -> Self {
        match level {
            Level::ERROR => Self::Error,
            Level::WARN => Self::Warn,
            Level::INFO => Self::Info,
            Level::DEBUG => Self::Debug,
            Level::TRACE => Self::Trace,
        }
    }

    const fn level(self) -> Level {
        match self {
            Self::Error => Level::ERROR,
            Self::Warn => Level::WARN,
            Self::Info => Level::INFO,
            Self::Debug => Level::DEBUG,
            Self::Trace => Level::TRACE,
        }
    }
}

/// A sink that could not take a line. The library ignores it.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SinkError {
    #[error("the log sink failed: {detail}")]
    Failed { detail: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for SinkError {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { detail: error.reason }
    }
}

/// Where the library's log lines go: the phone's logger.
#[uniffi::export(with_foreign)]
pub trait LogSink: Send + Sync {
    /// One log line. `target` is the Rust module it came from. Called on any thread.
    fn log(&self, level: LogLevel, target: String, message: String) -> Result<(), SinkError>;
}

/// Send the library's log lines at `level` and above to `sink`. Returns `false`, changing
/// nothing, when a sink is already set.
#[uniffi::export]
pub fn log_to(sink: Arc<dyn LogSink>, level: LogLevel) -> bool {
    let subscriber = Registry::default().with(Lines { sink, level: level.level() });
    tracing::subscriber::set_global_default(subscriber).is_ok()
}

/// The layer that hands each line to the sink.
struct Lines {
    sink: Arc<dyn LogSink>,
    /// The most verbose level passed on.
    level: Level,
}

thread_local! {
    /// Whether this thread is inside the sink, so a line it logs there is dropped.
    static IN_SINK: Cell<bool> = const { Cell::new(false) };
}

impl<S: Subscriber> Layer<S> for Lines {
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        *metadata.level() <= self.level
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(self.level.into())
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if IN_SINK.with(Cell::get) {
            return;
        }
        let mut line = Line::default();
        event.record(&mut line);
        let metadata = event.metadata();
        IN_SINK.with(|inside| inside.set(true));
        // A failing sink is the phone's to notice; the library logs on.
        let _ = self.sink.log(
            LogLevel::of(*metadata.level()),
            metadata.target().to_owned(),
            line.message + &line.fields,
        );
        IN_SINK.with(|inside| inside.set(false));
    }
}

/// An event's `message` field, then every other field as ` name=value`.
#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use tracing::Level;

    use super::LogLevel;

    #[test]
    fn log_level_round_trips_when_converted_through_tracing() {
        for level in [Level::ERROR, Level::WARN, Level::INFO, Level::DEBUG, Level::TRACE] {
            assert_eq!(LogLevel::of(level).level(), level);
        }
    }
}
