//! The operator's settings, kept where the phone keeps them.
//!
//! `wartui_app::settings` holds the schema, its rules and the one path that saves the settings;
//! its `SettingsStore` is the seam. The phone implements [`SettingsStore`] over app storage (the
//! keychain or `EncryptedSharedPreferences` when `secret` says it holds an API key), and
//! [`Settings`] holds the app's settings over it. The text is the same TOML a desktop keeps in
//! `wartui.toml`, so the rules are one.
//!
//! **The phone's store fails as a value.** A [`StoreFailure`] reaches the app layer as an
//! `io::Error` carrying its detail, as a file's failure does on a desktop. A store whose `place`
//! throws is called [`UNNAMED_PLACE`] in what the app reports, since a place is only a name.

use std::fmt;
use std::sync::Arc;

use wartui_app::settings::{self as app, SettingsError as AppError};

/// What a store whose `place` throws is called.
pub const UNNAMED_PLACE: &str = "settings";

/// The phone's store could not do what was asked.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StoreFailure {
    #[error("the settings store failed: {detail}")]
    Failed { detail: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for StoreFailure {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { detail: error.reason }
    }
}

/// Where the settings text is kept: app storage on a phone. Called on any thread.
#[uniffi::export(with_foreign)]
pub trait SettingsStore: Send + Sync {
    /// The stored text, or `None` when nothing is stored yet.
    fn load(&self) -> Result<Option<String>, StoreFailure>;
    /// Replace the stored text. `secret` says it holds an API key, so keep it from other apps
    /// and backups.
    fn save(&self, text: String, secret: bool) -> Result<(), StoreFailure>;
    /// What a notice calls this store.
    fn place(&self) -> Result<String, StoreFailure>;
}

/// Why the stored settings were refused. `place` is the store's, `detail` the reason.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SettingsError {
    /// The store could not give its text.
    #[error("reading {place}: {detail}")]
    Read { place: String, detail: String },
    /// The text is not TOML, or not this schema.
    #[error("parsing {place}: {detail}")]
    Malformed { place: String, detail: String },
    /// The settings break a rule.
    #[error("in {place}: {detail}")]
    Invalid { place: String, detail: String },
}

impl From<AppError> for SettingsError {
    fn from(error: AppError) -> Self {
        match error {
            AppError::Read { place, source } => Self::Read { place, detail: chain(&source) },
            AppError::Parse { place, source } => Self::Malformed { place, detail: chain(&source) },
            AppError::Invalid { place, source } => Self::Invalid { place, detail: chain(&source) },
        }
    }
}

/// `error` and every source under it, joined.
pub(crate) fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The operator's settings, and the one path that saves them.
#[derive(Debug, uniffi::Object)]
pub struct Settings {
    inner: app::Settings,
}

#[uniffi::export]
impl Settings {
    /// Load and validate what `store` holds, and save back through it.
    ///
    /// # Errors
    /// A store that cannot be read, or text the rules refuse.
    #[uniffi::constructor]
    pub fn open(store: Arc<dyn SettingsStore>) -> Result<Arc<Self>, SettingsError> {
        let store: Arc<dyn app::SettingsStore> = Arc::new(Store(store));
        let config = app::load(store.as_ref())?;
        let memory = config.bridge.memory();
        Ok(Arc::new(Self { inner: app::Settings::new(Some(store), config, memory) }))
    }

    /// Defaults, saved nowhere: for a simulated capture.
    #[uniffi::constructor]
    #[must_use]
    pub fn unsaved() -> Arc<Self> {
        Arc::new(Self { inner: app::Settings::default() })
    }

    /// Where they are saved, when they are.
    #[must_use]
    pub fn place(&self) -> Option<String> {
        self.inner.place()
    }
}

impl Settings {
    /// The app's settings, shared with every capture that saves through them.
    pub(crate) fn app(&self) -> app::Settings {
        self.inner.clone()
    }
}

/// The phone's store as the app layer's.
struct Store(Arc<dyn SettingsStore>);

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Store").finish_non_exhaustive()
    }
}

/// A store's failure as the app layer reads one.
fn io(failure: StoreFailure) -> std::io::Error {
    let StoreFailure::Failed { detail } = failure;
    std::io::Error::other(detail)
}

impl app::SettingsStore for Store {
    fn load(&self) -> std::io::Result<Option<String>> {
        self.0.load().map_err(io)
    }

    fn save(&self, text: &str, secret: bool) -> std::io::Result<()> {
        self.0.save(text.to_owned(), secret).map_err(io)
    }

    fn place(&self) -> String {
        self.0.place().unwrap_or_else(|_| UNNAMED_PLACE.to_owned())
    }
}
