//! `log_to`, the library's log lines handed to the phone's logger. Its own binary: the sink is set
//! once per process.

use std::sync::{Arc, Mutex, PoisonError};

use wartui_ffi::{LogLevel, LogSink, SinkError, log_to};

/// A sink that keeps every line.
#[derive(Default)]
struct Kept(Mutex<Vec<(LogLevel, String, String)>>);

impl LogSink for Kept {
    fn log(&self, level: LogLevel, target: String, message: String) -> Result<(), SinkError> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push((level, target, message));
        Ok(())
    }
}

#[test]
fn log_sink_receives_lines_when_level_allows() {
    let kept = Arc::new(Kept::default());
    assert!(log_to(kept.clone(), LogLevel::Info));
    assert!(!log_to(Arc::new(Kept::default()), LogLevel::Trace), "the sink is set once");

    tracing::info!(nodes = 3, peer = "loopback", "fleet heard");
    tracing::debug!("below the level");
    let lines = kept.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(
        lines,
        vec![(LogLevel::Info, "log".to_owned(), "fleet heard nodes=3 peer=loopback".to_owned())]
    );
}
