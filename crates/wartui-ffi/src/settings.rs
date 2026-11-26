//! The operator's settings, kept where the phone keeps them.
//!
//! `wartui_app::settings` holds the schema, its rules and the one path that saves the settings;
//! its `SettingsStore` is the seam. The phone implements [`SettingsStore`] over app storage, and
//! optionally [`SecretStore`] over the Keychain or the Keystore for the API keys, which
//! `wartui_app::settings::Split` keeps apart. [`Settings`] holds the app's settings over them. The
//! text is the same TOML a desktop keeps in `wartui.toml`, so the rules are one.
//!
//! **One edit, two ways in.** A [`SettingsEdit`] is a settings screen's rows. In a capture
//! `Session::apply` puts the moved rows in force and saves them; with none running
//! [`Settings::save`] saves the rows the host holds. Both convert the edit the same way first, and
//! a value the app's types refuse is a [`SteerError`] with nothing sent or saved. What the save
//! came to is a [`Saved`], typed as far as the app types it.
//!
//! **The phone's store fails as a value.** A [`StoreFailure`] reaches the app layer as an
//! `io::Error` carrying its detail, as a file's failure does on a desktop. A store whose `place`
//! throws is called [`UNNAMED_PLACE`] in what the app reports, since a place is only a name.

use std::fmt;
use std::sync::Arc;

use wartui_app::settings::{
    self as app, ApiKeys, GPS_RATE_HZ, Invalid, SSID_MAX_BYTES, SaveError, SerializeError,
    SettingsError as AppError, Split, SsidRefusal,
};
use wartui_app::view::PlannerShown;
use wartui_proto::fan::FanDuty;
use wartui_proto::mac;
use wartui_proto::plan::{ChannelPool, PlanSettings, SparseNodes};

use crate::session::SteerError;

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

/// Where the settings text is kept: app storage on a phone. Called on any thread, and free to
/// read [`Settings`] from inside a call. A `save` must not cause another save.
#[uniffi::export(with_foreign)]
pub trait SettingsStore: Send + Sync {
    /// The stored text, or `None` when nothing is stored yet.
    fn load(&self) -> Result<Option<String>, StoreFailure>;
    /// Replace the stored text. `secret` says it holds an API key, so keep it from other apps
    /// and backups. With a [`SecretStore`] it never does.
    fn save(&self, text: String, secret: bool) -> Result<(), StoreFailure>;
    /// What a notice calls this store.
    fn place(&self) -> Result<String, StoreFailure>;
}

/// Where the API keys are kept apart from the other settings: the Keychain or the Keystore. Its
/// text is the `[api-keys]` table alone. Called on any thread, as [`SettingsStore`] is: its
/// `save` inside a save, under the save's lock and no other, so it is free to read [`Settings`]
/// from inside a call, and a `save` must not cause another save.
#[uniffi::export(with_foreign)]
pub trait SecretStore: Send + Sync {
    /// The stored keys' text, or `None` when none is stored.
    fn load(&self) -> Result<Option<String>, StoreFailure>;
    /// Replace the stored keys' text.
    fn save(&self, text: String) -> Result<(), StoreFailure>;
}

/// A rule the settings break.
#[uniffi::remote(Enum)]
pub enum Invalid {
    /// `[gps] bridge-rate-hz` is outside [`settings_limits`]' range.
    GpsRate(u8),
    /// A bridge address is remembered beside `remember = false`.
    ForgottenMac,
    /// A USB serial number is remembered beside `remember = false`.
    ForgottenSerial,
    /// A Bluetooth address is remembered beside `remember = false`.
    ForgottenBluetoothAddress,
    /// An excluded SSID is empty.
    EmptySsid,
    /// An excluded SSID is longer than [`settings_limits`]' `ssid_max_bytes`.
    LongSsid(String),
    /// An excluded SSID is listed twice.
    ListedSsid(String),
}

/// Why an SSID cannot join the excluded list.
#[uniffi::remote(Enum)]
pub enum SsidRefusal {
    /// Nothing typed. A hidden network's empty SSID is not excludable.
    Empty,
    /// Longer than [`settings_limits`]' `ssid_max_bytes`, so no sighting could match it.
    TooLong,
    /// Already in the list.
    Listed,
}

/// The `[api-keys]` table. An empty string means not set.
#[uniffi::remote(Record)]
pub struct ApiKeys {
    /// WDGWars' API key.
    pub wdgwars: String,
    /// WiGLE's API Name, as its account page gives it.
    pub wigle_name: String,
    /// WiGLE's API Token, as its account page gives it.
    pub wigle_token: String,
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
    #[error("in {place}: {reason}")]
    Invalid { place: String, reason: Invalid },
}

impl From<AppError> for SettingsError {
    fn from(error: AppError) -> Self {
        match error {
            AppError::Read { place, source } => Self::Read { place, detail: chain(&source) },
            AppError::Parse { place, source } => Self::Malformed { place, detail: chain(&source) },
            AppError::Invalid { place, source } => Self::Invalid { place, reason: source },
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

/// A settings screen's rows, as the app's `SettingsEdit` takes them. Each row the bridge holds,
/// and the receiver's rate, defaults to unmoved.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SettingsEdit {
    /// `Some` only when the row moved, as each row the bridge holds.
    #[uniffi(default)]
    pub pool: Option<ChannelPool>,
    /// The planner and its sparse node count, `Some` when either moved.
    #[uniffi(default)]
    pub planner: Option<PlannerShown>,
    /// The lid fan's duty, in percent, within [`settings_limits`].
    #[uniffi(default)]
    pub fan_duty_pct: Option<u8>,
    /// Whether Bluetooth is in the scan.
    #[uniffi(default)]
    pub bluetooth: Option<bool>,
    /// Whether a phone may link as a WardriveESP board.
    #[uniffi(default)]
    pub wardrive_esp: Option<bool>,
    /// Whether the bridge logs to its SD card.
    #[uniffi(default)]
    pub sd_log: Option<bool>,
    /// The bridge receiver's rate, in hertz, within [`settings_limits`]. Unmoved is the rate in
    /// force.
    #[uniffi(default)]
    pub gps_rate_hz: Option<u8>,
    /// Whether the bridge connected to is remembered.
    pub remember_bridge: bool,
    /// Whether a capture holds its scan for a GPS fix, from the next capture.
    pub wait_for_fix: bool,
    /// The keys as typed, which are trimmed before they are saved.
    pub api_keys: ApiKeys,
    /// The list in force, which is saved and not sent.
    pub excluded_ssids: Vec<String>,
}

impl SettingsEdit {
    /// The app's edit, or the first value its types refuse.
    pub(crate) fn into_app(self) -> Result<app::SettingsEdit, SteerError> {
        let fan = match self.fan_duty_pct {
            Some(pct) => Some(FanDuty::new(pct).ok_or(SteerError::FanDuty { pct })?),
            None => None,
        };
        let planner = match self.planner {
            Some(PlannerShown { planner, sparse_nodes: Some(count) }) => {
                let sparse_nodes =
                    SparseNodes::fixed(count).ok_or(SteerError::SparseNodes { count })?;
                Some(PlanSettings { planner, sparse_nodes })
            }
            Some(PlannerShown { planner, sparse_nodes: None }) => {
                Some(PlanSettings { planner, sparse_nodes: SparseNodes::AUTO })
            }
            None => None,
        };
        if let Some(hz) = self.gps_rate_hz.filter(|hz| !GPS_RATE_HZ.contains(hz)) {
            return Err(SteerError::GpsRate { hz });
        }
        Ok(app::SettingsEdit {
            pool: self.pool,
            planner,
            fan,
            bluetooth: self.bluetooth,
            wardrive_esp: self.wardrive_esp,
            sd_log: self.sd_log,
            gps_rate: self.gps_rate_hz,
            remember_bridge: self.remember_bridge,
            wait_for_fix: self.wait_for_fix,
            api_keys: self.api_keys,
            excluded_ssids: self.excluded_ssids,
        })
    }
}

/// What a save came to.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Saved {
    /// Saved to the store, which calls itself `place`.
    At { place: String },
    /// There is no store: the settings are [`Settings::unsaved`].
    Nowhere,
    /// The settings break a rule, so nothing was written.
    Invalid { reason: Invalid },
    /// TOML cannot hold the settings, so nothing was written.
    Unserializable { detail: String },
    /// The store failed.
    Store { detail: String },
}

impl From<Result<String, SaveError>> for Saved {
    fn from(saved: Result<String, SaveError>) -> Self {
        match saved {
            Ok(place) => Self::At { place },
            Err(SaveError::Nowhere) => Self::Nowhere,
            Err(SaveError::Invalid(SerializeError::Invalid(reason))) => Self::Invalid { reason },
            Err(SaveError::Invalid(SerializeError::Toml(error))) => {
                Self::Unserializable { detail: chain(&error) }
            }
            Err(SaveError::Store(error)) => Self::Store { detail: chain(&error) },
        }
    }
}

/// The settings as last loaded or saved, for a settings screen outside a capture.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SavedSettings {
    /// The Wi-Fi networks never captured.
    pub excluded_ssids: Vec<String>,
    /// Whether the bridge connected to is remembered.
    pub remember_bridge: bool,
    /// The bridge remembered, its whole address as `AA:BB:CC:DD:EE:FF`.
    pub bridge_mac: Option<String>,
    /// The key the phone's OS knows the remembered bridge by.
    pub bluetooth_address: Option<String>,
    /// The bridge receiver's rate, in hertz, or `None` for the receiver's default.
    pub gps_rate_hz: Option<u8>,
    /// Whether a capture holds its scan for a GPS fix.
    pub wait_for_fix: bool,
    /// The API keys.
    pub api_keys: ApiKeys,
}

/// The ranges a settings screen offers, so the edit it builds is one the app takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SettingsLimits {
    /// The slowest bridge receiver rate, in hertz.
    pub gps_rate_hz_min: u8,
    /// The fastest bridge receiver rate, in hertz.
    pub gps_rate_hz_max: u8,
    /// The most bytes an excluded SSID holds.
    pub ssid_max_bytes: u32,
    /// The highest lid fan duty, in percent; the lowest is 0.
    pub fan_duty_pct_max: u8,
    /// The most sparse nodes the weighted planner takes; the fewest is 1, and `None` is
    /// automatic.
    pub sparse_nodes_max: u8,
}

/// The ranges a settings screen offers.
#[uniffi::export]
#[must_use]
pub fn settings_limits() -> SettingsLimits {
    SettingsLimits {
        gps_rate_hz_min: *GPS_RATE_HZ.start(),
        gps_rate_hz_max: *GPS_RATE_HZ.end(),
        ssid_max_bytes: u32::try_from(SSID_MAX_BYTES).unwrap_or(u32::MAX),
        fan_duty_pct_max: FanDuty::FULL.get(),
        sparse_nodes_max: SparseNodes::MAX,
    }
}

/// Why `ssid` cannot join the excluded list after `listed`, or `None` when it can. A load refuses
/// the same entries.
#[uniffi::export]
#[must_use]
pub fn check_excluded_ssid(ssid: String, listed: Vec<String>) -> Option<SsidRefusal> {
    app::check_excluded_ssid(&ssid, &listed).err()
}

/// The operator's settings, and the one path that saves them.
#[derive(Debug, uniffi::Object)]
pub struct Settings {
    inner: app::Settings,
}

#[uniffi::export]
impl Settings {
    /// Load and validate what `store` holds, with the API keys in `secrets` when one is given,
    /// and save back the same way.
    ///
    /// # Errors
    /// A store that cannot be read, or text the rules refuse.
    #[uniffi::constructor]
    pub fn open(
        store: Arc<dyn SettingsStore>,
        secrets: Option<Arc<dyn SecretStore>>,
    ) -> Result<Arc<Self>, SettingsError> {
        let store: Arc<dyn app::SettingsStore> = Arc::new(Store(store));
        let store: Arc<dyn app::SettingsStore> = match secrets {
            Some(secrets) => Arc::new(Split::new(store, Arc::new(Secrets(secrets)))),
            None => store,
        };
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

    /// The settings as last loaded or saved, and the bridge remembered now.
    #[must_use]
    pub fn saved(&self) -> SavedSettings {
        let saved = self.inner.saved();
        let memory = self.inner.memory();
        SavedSettings {
            excluded_ssids: saved.excluded_ssids,
            remember_bridge: memory.is_enabled(),
            bridge_mac: memory.recall().map(|mac| mac::full(&mac).to_string()),
            bluetooth_address: memory.recall_bluetooth(),
            gps_rate_hz: saved.gps.bridge_rate_hz,
            wait_for_fix: saved.gps.waits_for_fix(),
            api_keys: saved.api_keys,
        }
    }

    /// Save the rows the host holds, with no capture running. Rows a bridge holds are ignored,
    /// once the edit converts.
    ///
    /// # Errors
    /// A value the app's types refuse, and then nothing is saved.
    pub fn save(&self, edit: SettingsEdit) -> Result<Saved, SteerError> {
        let edit = edit.into_app()?;
        Ok(self.inner.save(&edit).into())
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

/// The phone's secret store as the app layer's.
struct Secrets(Arc<dyn SecretStore>);

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Secrets").finish_non_exhaustive()
    }
}

impl app::SecretStore for Secrets {
    fn load(&self) -> std::io::Result<Option<String>> {
        self.0.load().map_err(io)
    }

    fn save(&self, text: &str) -> std::io::Result<()> {
        self.0.save(text.to_owned()).map_err(io)
    }
}
