//! The settings an operator wants to stop typing every run, and the rules any stored copy follows.
//!
//! [`Config`] is everything the settings hold: the excluded SSIDs, the `[bridge]` remembered and
//! the link that reaches it, the `[gps]` rate and wait, and the `[api-keys]`. Every table denies
//! unknown fields, so a typo is caught rather than silently ignored.
//!
//! **The operator's own decision**, so a mistake is refused rather than absorbed: [`parse`]
//! refuses an unknown key or an out-of-range value and says which, and [`load`] names the store
//! too. [`serialize`] checks the same rules, so a save never leaves text a load would refuse.
//!
//! The bridge holds the pool, the planner, the fan duty, the scan mode and the SD log setting, and
//! boots into them, so the settings have no key for any of them. Text that names the pool or a
//! `[bluetooth]` table is refused with a hint saying where that setting is set.
//!
//! [`SettingsStore`] is the seam: where the text is kept, a file on a desktop
//! (`wartui_desktop::config`) and app storage on a phone. [`Settings`] holds the copy last loaded
//! or saved, and is the one path that writes it.
//!
//! This lives in `wartui-app` rather than `wartui-core`: the core crate parses no operator input,
//! and the settings are operator input just like a flag is.

mod saved;

use std::ops::RangeInclusive;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use wartui_proto::mac::{self, Mac};

pub use saved::{Applied, Busy, SaveError, Settings, SettingsEdit};
pub(crate) use saved::{apply, apply_excluded};

/// Everything the settings hold. Add a field and a table to grow it; every struct denies unknown
/// fields, so a typo is caught rather than silently ignored. [`serialize`] omits a table with
/// nothing set, except `[api-keys]`, which is always written so a hand edit only has to fill in
/// the quotes.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// The root `excluded-ssids` key: Wi-Fi networks never captured, matched
    /// byte-for-byte. Declared before the tables, since TOML root keys must come first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_ssids: Vec<String>,
    #[serde(default, skip_serializing_if = "Bridge::is_empty")]
    pub bridge: Bridge,
    #[serde(default, skip_serializing_if = "Gps::is_empty")]
    pub gps: Gps,
    #[serde(default)]
    pub api_keys: ApiKeys,
}

/// The `[bridge]` table: which link reaches the bridge, whether `run` and `flash-bridge`
/// remember the bridge, and the bridge remembered, so the next start opens that board rather
/// than sweeping. `link` absent means USB. `remember` absent means on. `mac` is written the way
/// `wartui ports` prints a MAC, `serial` is the USB serial number of the CH343 it answered on,
/// which the OS reports before anything is opened, and `bluetooth-address` is the address the OS
/// reports for its C6, or on macOS the identifier CoreBluetooth gives it. Those three are refused
/// beside `remember = false`, which forgets them.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Bridge {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "write_mac",
        deserialize_with = "read_bridge_mac"
    )]
    pub mac: Option<Mac>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bluetooth_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

impl Bridge {
    /// Whether no key is set, so [`serialize`] writes no `[bridge]` table at all.
    fn is_empty(&self) -> bool {
        self.link.is_none()
            && self.remember.is_none()
            && self.mac.is_none()
            && self.serial.is_none()
            && self.bluetooth_address.is_none()
    }
}

/// Which link reaches the bridge: `[bridge] link`, and a front end's choice of link.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Link {
    /// The board's CH343, over USB.
    #[default]
    Usb,
    /// The board's C6, over Bluetooth, to a PC paired with it.
    Bluetooth,
}

impl From<Link> for wartui_core::record::HostLink {
    fn from(link: Link) -> Self {
        match link {
            Link::Usb => Self::Usb,
            Link::Bluetooth => Self::Bluetooth,
        }
    }
}

/// The `[gps]` table: how often a bridge's GPS receiver reports a fix, in whole hertz
/// within [`GPS_RATE_HZ`], and whether `run` holds the scan until a receiver holds a fix.
/// `bridge-rate-hz` absent means [`wartui_core::ubx::DEFAULT_RATE_HZ`], and
/// `wait-for-fix` absent means wait. [`crate::session::Controls::apply`] writes both, and leaves
/// each as the stored copy had it while its row is unmoved.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Gps {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_rate_hz: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_fix: Option<bool>,
}

impl Gps {
    /// Whether `run` waits for a GPS fix before it starts the scan: absent means wait.
    #[must_use]
    pub fn waits_for_fix(&self) -> bool {
        self.wait_for_fix.unwrap_or(true)
    }

    /// Whether no key is set, so [`serialize`] writes no `[gps]` table at all.
    fn is_empty(&self) -> bool {
        self.bridge_rate_hz.is_none() && self.wait_for_fix.is_none()
    }
}

/// The `[api-keys]` table. An empty string means not set, and is written anyway.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ApiKeys {
    #[serde(default)]
    pub wdgwars: String,
    /// WiGLE's API Name, as its account page gives it.
    #[serde(default)]
    pub wigle_name: String,
    /// WiGLE's API Token, as its account page gives it.
    #[serde(default)]
    pub wigle_token: String,
}

impl ApiKeys {
    /// Whether any key is set, so the store holding them keeps them from other users.
    #[must_use]
    pub fn any_set(&self) -> bool {
        !self.wdgwars.is_empty() || !self.wigle_name.is_empty() || !self.wigle_token.is_empty()
    }
}

fn write_mac<S: Serializer>(mac: &Option<Mac>, serializer: S) -> Result<S::Ok, S::Error> {
    match mac {
        Some(mac) => serializer.collect_str(&mac::full(mac)),
        None => serializer.serialize_none(),
    }
}

fn read_bridge_mac<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Mac>, D::Error> {
    read_mac("bridge.mac", deserializer)
}

/// A MAC at `key`, named in the error when the text is not one.
fn read_mac<'de, D: Deserializer<'de>>(
    key: &str,
    deserializer: D,
) -> Result<Option<Mac>, D::Error> {
    let text = String::deserialize(deserializer)?;
    mac::parse(&text).map(Some).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "{key} = \"{text}\" is not a MAC written like AA:BB:CC:DD:EE:FF"
        ))
    })
}

/// Valid `[gps] bridge-rate-hz` values: the rates the receiver configuration takes.
pub const GPS_RATE_HZ: RangeInclusive<u8> = wartui_core::ubx::RATE_HZ;

/// Each excluded SSID as the bytes a sighting's SSID is matched against.
pub fn excluded_ssid_bytes(ssids: &[String]) -> impl Iterator<Item = Vec<u8>> + '_ {
    ssids.iter().map(|ssid| ssid.as_bytes().to_vec())
}

/// Why a [`Config`] is refused: a value its key does not take, or keys that contradict each
/// other.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    #[error(
        "gps.bridge-rate-hz = {0} is outside {low} to {high} Hz",
        low = GPS_RATE_HZ.start(),
        high = GPS_RATE_HZ.end()
    )]
    GpsRate(u8),
    #[error("bridge.mac is set while bridge.remember = false, which forgets it")]
    ForgottenMac,
    #[error("bridge.serial is set while bridge.remember = false, which forgets it")]
    ForgottenSerial,
    #[error("bridge.bluetooth-address is set while bridge.remember = false, which forgets it")]
    ForgottenBluetoothAddress,
    #[error("excluded-ssids: an entry is empty")]
    EmptySsid,
    #[error(
        "excluded-ssids: \"{0}\" is longer than {max} bytes, the most an SSID holds",
        max = SSID_MAX_BYTES
    )]
    LongSsid(String),
    #[error("excluded-ssids: \"{0}\" is listed twice")]
    ListedSsid(String),
}

impl Config {
    /// Check the rules no table's type can: the rate's range, no remembered bridge beside
    /// `remember = false`, and the excluded list's entries.
    ///
    /// # Errors
    /// The first rule broken.
    pub fn validate(&self) -> Result<(), Invalid> {
        if let Some(rate) = self.gps.bridge_rate_hz
            && !GPS_RATE_HZ.contains(&rate)
        {
            return Err(Invalid::GpsRate(rate));
        }
        if self.bridge.remember == Some(false) {
            if self.bridge.mac.is_some() {
                return Err(Invalid::ForgottenMac);
            }
            if self.bridge.serial.is_some() {
                return Err(Invalid::ForgottenSerial);
            }
            if self.bridge.bluetooth_address.is_some() {
                return Err(Invalid::ForgottenBluetoothAddress);
            }
        }
        for (index, ssid) in self.excluded_ssids.iter().enumerate() {
            match check_excluded_ssid(ssid, &self.excluded_ssids[..index]) {
                Ok(()) => {}
                Err(SsidRefusal::Empty) => return Err(Invalid::EmptySsid),
                Err(SsidRefusal::TooLong) => return Err(Invalid::LongSsid(ssid.clone())),
                Err(SsidRefusal::Listed) => return Err(Invalid::ListedSsid(ssid.clone())),
            }
        }
        Ok(())
    }
}

/// The most bytes an SSID holds, per 802.11.
pub const SSID_MAX_BYTES: usize = 32;

/// Why an SSID cannot join the excluded list. A load and a front end's list refuse the same
/// entries, so a front end never saves a list a load would refuse; each words the refusal its own
/// way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsidRefusal {
    /// Nothing typed. A hidden network's empty SSID is not excludable.
    Empty,
    /// Over [`SSID_MAX_BYTES`], so no sighting could match it.
    TooLong,
    /// Already in the list.
    Listed,
}

/// Whether `ssid` can join `listed`, the entries before it.
///
/// # Errors
/// Why it cannot.
pub fn check_excluded_ssid(ssid: &str, listed: &[String]) -> Result<(), SsidRefusal> {
    if ssid.is_empty() {
        Err(SsidRefusal::Empty)
    } else if ssid.len() > SSID_MAX_BYTES {
        Err(SsidRefusal::TooLong)
    } else if listed.iter().any(|entry| entry == ssid) {
        Err(SsidRefusal::Listed)
    } else {
        Ok(())
    }
}

/// Why text is not settings at all: not TOML, or not this schema.
#[derive(Debug, thiserror::Error)]
pub enum Malformed {
    /// The parser's own error, which names the line. Boxed, since it is large.
    #[error(transparent)]
    Toml(Box<toml::de::Error>),
    /// The text holds a setting the bridge keeps instead. The parser's error after the hint
    /// names the line.
    #[error("{hint}")]
    BridgeSetting {
        hint: &'static str,
        #[source]
        source: Box<toml::de::Error>,
    },
}

/// Why [`parse`] refused text.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Malformed(#[from] Malformed),
    #[error(transparent)]
    Invalid(#[from] Invalid),
}

/// Why [`serialize`] wrote nothing.
#[derive(Debug, thiserror::Error)]
pub enum SerializeError {
    #[error("the settings to save are invalid")]
    Invalid(#[source] Invalid),
    #[error("serializing the settings")]
    Toml(#[source] toml::ser::Error),
}

/// Why [`load`] refused the stored settings. Each names the store's place, and the chain of
/// sources after it says why.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("reading {place}")]
    Read {
        place: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {place}")]
    Parse {
        place: String,
        #[source]
        source: Malformed,
    },
    #[error("in {place}")]
    Invalid {
        place: String,
        #[source]
        source: Invalid,
    },
}

/// The settings `text` holds: parsed, then checked with [`Config::validate`].
///
/// # Errors
/// Text that is not TOML or not this schema, with a hint when it holds a setting the bridge
/// keeps, or settings that break a rule.
pub fn parse(text: &str) -> Result<Config, ParseError> {
    let config: Config = toml::from_str(text).map_err(|source| {
        let source = Box::new(source);
        match bridge_setting(text) {
            Some(hint) => Malformed::BridgeSetting { hint, source },
            None => Malformed::Toml(source),
        }
    })?;
    config.validate()?;
    Ok(config)
}

/// The text `config` is stored as, once [`Config::validate`] passes it.
///
/// # Errors
/// Settings that break a rule, or that TOML cannot hold.
pub fn serialize(config: &Config) -> Result<String, SerializeError> {
    config.validate().map_err(SerializeError::Invalid)?;
    toml::to_string(config).map_err(SerializeError::Toml)
}

/// Why text is refused, when it holds a setting the bridge keeps instead: the pool, and the
/// Bluetooth scan, whose node the bridge picks. The parse error that follows names the line.
fn bridge_setting(text: &str) -> Option<&'static str> {
    let table: toml::Table = text.parse().ok()?;
    if table.contains_key("pool") {
        Some(
            "the bridge holds the channel pool, so wartui.toml has no `pool`: set it with `wartui pool`",
        )
    } else if table.contains_key("bluetooth") {
        Some(
            "the bridge picks the node that scans Bluetooth, so wartui.toml has no `[bluetooth]`: \
             add Bluetooth to the scan with `wartui scan start --bluetooth`, or the `bluetooth` \
             row in `run`'s settings (`c`)",
        )
    } else {
        None
    }
}

/// Where the settings text is kept: a file on a desktop, app storage on a phone.
pub trait SettingsStore: std::fmt::Debug + Send + Sync {
    /// The stored text, or `None` when nothing is stored yet. [`load`] names the place in an
    /// error.
    ///
    /// # Errors
    /// The store's own, when it holds text it cannot give.
    fn load(&self) -> std::io::Result<Option<String>>;
    /// Replace the stored text. `secret` says it holds an API key, so the store keeps it from other
    /// users.
    ///
    /// # Errors
    /// The step that failed and why, since a save can take several.
    fn save(&self, text: &str, secret: bool) -> std::io::Result<()>;
    /// What a notice calls this store: a path on a desktop.
    fn place(&self) -> String;
}

/// The settings `store` holds, or [`Config::default`] when it holds nothing yet.
///
/// # Errors
/// A store that cannot be read, or text [`parse`] refuses, each naming the store's place.
pub fn load(store: &dyn SettingsStore) -> Result<Config, SettingsError> {
    let text = match store.load() {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(Config::default()),
        Err(source) => return Err(SettingsError::Read { place: store.place(), source }),
    };
    parse(&text).map_err(|error| match error {
        ParseError::Malformed(source) => SettingsError::Parse { place: store.place(), source },
        ParseError::Invalid(source) => SettingsError::Invalid { place: store.place(), source },
    })
}
