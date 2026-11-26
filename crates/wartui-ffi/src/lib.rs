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
//! | | [`bridges_nearby`], every bridge the radio hears or the OS lists |
//! | [`SettingsStore`] | [`Settings`]: `open`, `unsaved`, `place`, `saved`, `save` |
//! | [`SecretStore`] | [`settings_limits`], [`check_excluded_ssid`] |
//! | [`LogSink`] | [`log_to`] |
//! | [`ReadyListener`] | [`Link`]: `simulated`, `bluetooth`, `is_simulated`, `ready`, `close` |
//! | | [`LocationFeed`]: `push`, `lost`, `wanted`, `next_wanted`, `state` |
//! | | [`max_fix_accuracy_m`], the coarsest fix a `LocationFeed` takes |
//! | | [`HealthFeed`]: `set` |
//! | | [`Session`]: `start`, `view`, `next_view`, `show_sky`, `stop` |
//! | | [`Session`]'s steering: `apply`, `apply_excluded`, `start_without_fix` |
//! | | [`Session`]'s bridge commands: `power_cycle`, `format_sd` |
//! | | [`export_wigle_csv`] |
//! | [`UploadListener`] | [`prepare_upload`], then [`PreparedUpload`]: `shown`, `send` |
//! | | [`NodeImage`], [`BridgeImage`]: `parse`; [`fetch_node_image`], [`fetch_bridge_image`] |
//! | [`NodeUpdateListener`] | [`Link`]'s node update: `hear_nodes`, then `update_nodes` |
//! | | [`NodesHeard`]: `targets`, `boards`, `plan`; [`NodePlan`]: `rows`, `chosen` |
//! | [`BridgeUpdateListener`] | [`Link`]'s bridge update: `update_bridge` |
//! | | [`Stopper`]: `new`, `stop`, for either update |
//!
//! Kotlin calls `Link::close` `disconnect`. Both languages call the rule enum `Invalid`
//! `BrokenRule`, the upload's `Missing` `MissingCredentials` and its `Unfollowed`
//! `JobUnfollowed` (`uniffi.toml`).
//!
//! **Threads.** Calls arrive on any thread, a new one each time on Kotlin. Every task runs on the
//! library's one runtime (`runtime`), and no exported function blocks on it: an exported
//! `async fn` awaits a `tokio::sync` primitive, or the handle of work it spawned there.
//!
//! **A call into the phone's code returns at once and never takes the library down.** Each
//! method the phone implements returns a `Result`, so an exception it throws is a value here, and
//! each caller says what it does with one. No lock is held across such a call, but one: a save
//! is in progress across [`SettingsStore::save`] and [`SecretStore::save`], so a `save` that saves
//! the settings itself deadlocks (`wartui_app::settings`, `saved.rs`). A request the phone answers
//! later carries an id, and the answer comes through the object it was asked of.
//!
//! **Typed values, no sentences**, as in `wartui-app`: an error or a state crosses as an enum the
//! phone words. A `String` is a datum, or the detail of a failure from a layer below, in a field
//! named `detail`. An object's methods take `&self`, so what one consumes is held in a
//! `Mutex<Option<_>>` and a second call is a typed error.

mod export;
mod feeds;
mod link;
mod log;
mod radio;
mod runtime;
mod session;
mod settings;
#[cfg(feature = "testing")]
mod testing;
mod update;
mod upload;
mod view;

pub use export::{ExportError, ExportOptions, Exported, ExportedNode, export_wigle_csv};
pub use feeds::{HealthFeed, LocationFeed, max_fix_accuracy_m};
pub use link::{Announced, Link, LinkError, ReadyError, ReadyListener};
pub use log::{LogLevel, LogSink, SinkError, log_to};
pub use radio::{
    BridgeNearby, Completion, DriverError, Heard, LinkUuids, NearbyError, Radio, RadioDriver,
    bridges_nearby, link_uuids,
};
pub use session::{
    Applied, CaptureConfig, CaptureEnd, PositionSources, Session, StartError, SteerError,
    StopError, Viewed,
};
pub use settings::{
    Saved, SavedSettings, SecretStore, Settings, SettingsEdit, SettingsError, SettingsLimits,
    SettingsStore, StoreFailure, UNNAMED_PLACE, check_excluded_ssid, settings_limits,
};
#[cfg(feature = "testing")]
pub use testing::{WireSim, testing_node_image};
pub use update::{
    BridgeImage, BridgeUpdateFailed, BridgeUpdateListener, BridgeUpdateProgress, BridgeUpdated,
    CutShort, FetchError, ImageError, LinkKind, NodeAction, NodeCapabilities, NodeEnded,
    NodeFailure, NodeImage, NodeOutcome, NodePlan, NodeTarget, NodeUpdateListener,
    NodeUpdateProgress, NodesFailed, NodesHeard, NodesUpdated, PlanRow, QueueFailure,
    ReleaseFailure, SelectFailed, SenderFailure, SessionProgress, Stopper, UpdateError,
    fetch_bridge_image, fetch_node_image,
};
#[cfg(feature = "testing")]
pub use update::{fetch_bridge_image_from, fetch_node_image_from};
pub use upload::{
    PreparedUpload, PreviousUpload, SiteOutcome, SitePrepared, UploadError, UploadListener,
    UploadOptions, UploadOutcome, UploadProgress, UploadRefusal, UploadSendFailure, UploadShown,
    prepare_upload,
};
#[cfg(feature = "testing")]
pub use upload::{UploadEndpoints, prepare_upload_at};

uniffi::setup_scaffolding!();
