//! The library a phone app loads: `wartui-app` through UniFFI, for Kotlin and Swift.
//!
//! This is the only crate that names UniFFI. The app's types stay free of it and cross as remote
//! declarations here (`#[uniffi::remote(Record)]`, `#[uniffi::remote(Enum)]`), which repeat each
//! field list, so a field added on one side only fails to compile. It holds no `unsafe` of its
//! own: UniFFI's scaffolding is the only code at the C boundary.
//!
//! | The phone implements | The phone calls |
//! |---|---|
//! | [`RadioDriver`] | [`Radio`]: `heard`, `completed`, `notified`, `dropped` |
//! | [`SettingsStore`] | [`Settings`]: `open`, `unsaved`, `place` |
//! | [`LogSink`] | [`log_to`] |
//! | | [`Link`]: `simulated`, `bluetooth`, `is_simulated`, `close` (`disconnect` in Kotlin) |
//! | | [`LocationFeed`]: `push`, `lost`, `wanted`, `next_wanted`, `state` |
//! | | [`HealthFeed`]: `set` |
//! | | [`Session`]: `start`, `view`, `next_view`, `show_sky`, `stop` |
//!
//! **Threads.** Calls arrive on any thread, a new one each time on Kotlin. Every task runs on the
//! library's one runtime (`runtime`), and no exported function blocks on it: an exported
//! `async fn` awaits a `tokio::sync` primitive, or the handle of work it spawned there.
//!
//! **A call into the phone's code returns at once and never takes the library down.** Each
//! method the phone implements returns a `Result`, so an exception it throws is a value here, and
//! each caller says what it does with one. No lock is held across such a call. A request the
//! phone answers later carries an id, and the answer comes through the object it was asked of.
//!
//! **Typed values, no sentences**, as in `wartui-app`: an error or a state crosses as an enum the
//! phone words. A `String` is a datum, or the detail of a failure from a layer below, in a field
//! named `detail`. An object's methods take `&self`, so what one consumes is held in a
//! `Mutex<Option<_>>` and a second call is a typed error.

mod feeds;
mod link;
mod log;
mod radio;
mod runtime;
mod session;
mod settings;
#[cfg(feature = "testing")]
mod testing;
mod view;

pub use feeds::{HealthFeed, LocationFeed};
pub use link::{Link, LinkError};
pub use log::{LogLevel, LogSink, SinkError, log_to};
pub use radio::{Completion, DriverError, Heard, LinkUuids, Radio, RadioDriver, link_uuids};
pub use session::{
    CaptureConfig, CaptureEnd, PositionSources, Session, StartError, StopError, Viewed,
};
pub use settings::{Settings, SettingsError, SettingsStore, StoreFailure, UNNAMED_PLACE};
#[cfg(feature = "testing")]
pub use testing::WireSim;

uniffi::setup_scaffolding!();
