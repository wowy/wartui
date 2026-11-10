//! `wartui.toml` — settings an operator wants to stop typing every run.
//!
//! **The operator's own decision**, so a mistake in it is refused rather than absorbed:
//! an unknown key or an out-of-range value stops `run`, `export`, `analyze`, `upload`
//! and `flash-bridge` from starting, and names the file and the problem.
//! `--config` is one of each's own arguments. Within `run`, the file beats the built-in
//! default. `export`, `analyze` and `upload` read the excluded SSIDs, `upload` the API key
//! too, and none of them creates or saves the file.
//!
//! **The remembered bridge is the one value other commands read.** `status`, `sniff`,
//! `reset` and `ota` take `[bridge] mac` and `serial` through [`remembered_bridge`] to pick
//! or order boards, and a missing or broken file is no bridge remembered there, so a bad
//! `wartui.toml` cannot stop a command that checks the link. `flash-bridge` takes it
//! through [`load`] and refuses a broken file: which board it writes firmware over
//! depends on it. `flash-fleet` never reads the file.
//!
//! **Two commands write the bridge.** `run` creates the file once when it is absent,
//! holding only the empty API key, so a first-time operator has a place to paste one. It
//! never rewrites an existing file at startup. Its view does, through one save path: the
//! settings modal, and a change to the bridge the transport remembers.
//! `flash-bridge` saves the board it found and flashed as the bridge, unless remembering
//! is off, so `run` and the next `flash-bridge` open the new bridge first. A board named
//! with `--bridge` is often some other device, and is not saved.
//!
//! This lives in `crates/wartui` rather than `wartui-core`: the core crate parses no
//! arguments, and a config file is operator input just like a flag is.

use std::ffi::OsStr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use wartui_proto::mac::{self, Mac};

/// Everything `wartui.toml` can hold. Add a field and a table to grow it; every
/// struct denies unknown fields, so a typo in the file is caught rather than
/// silently ignored. `save` omits a table with nothing set, except `[api-keys]`,
/// which is always written so a hand edit only has to fill in the quotes.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
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

/// The `[bridge]` table: whether `run` and `flash-bridge` remember the bridge, and the
/// bridge remembered, so the next start opens that board rather than sweeping.
/// `remember` absent means on. `mac` is written the way `wartui ports` prints a MAC, and
/// `serial` is the USB serial number of the CH343 it answered on, which the OS reports
/// before anything is opened. Both are refused beside `remember = false`, which forgets them.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
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
}

impl Bridge {
    /// Whether no key is set, so `save` writes no `[bridge]` table at all.
    fn is_empty(&self) -> bool {
        self.remember.is_none() && self.mac.is_none() && self.serial.is_none()
    }
}

/// The `[gps]` table: how often a bridge's GPS receiver reports a fix, in whole hertz
/// within [`GPS_RATE_HZ`]. Absent means [`wartui_core::ubx::DEFAULT_RATE_HZ`].
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Gps {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_rate_hz: Option<u8>,
}

impl Gps {
    /// Whether the key is unset, so `save` writes no `[gps]` table at all.
    fn is_empty(&self) -> bool {
        self.bridge_rate_hz.is_none()
    }
}

/// The `[api-keys]` table. An empty string means not set, and is written anyway.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ApiKeys {
    #[serde(default)]
    pub wdgwars: String,
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

impl Config {
    fn validate(&self) -> Result<()> {
        if let Some(rate) = self.gps.bridge_rate_hz
            && !GPS_RATE_HZ.contains(&rate)
        {
            bail!(
                "gps.bridge-rate-hz = {rate} is outside {} to {} Hz",
                GPS_RATE_HZ.start(),
                GPS_RATE_HZ.end()
            );
        }
        if self.bridge.remember == Some(false) && self.bridge.mac.is_some() {
            bail!("bridge.mac is set while bridge.remember = false, which forgets it");
        }
        if self.bridge.remember == Some(false) && self.bridge.serial.is_some() {
            bail!("bridge.serial is set while bridge.remember = false, which forgets it");
        }
        for (index, ssid) in self.excluded_ssids.iter().enumerate() {
            match check_excluded_ssid(ssid, &self.excluded_ssids[..index]) {
                Ok(()) => {}
                Err(SsidRefusal::Empty) => bail!("excluded-ssids: an entry is empty"),
                Err(SsidRefusal::TooLong) => bail!(
                    "excluded-ssids: \"{ssid}\" is longer than {SSID_MAX_BYTES} bytes, the most an \
                     SSID holds"
                ),
                Err(SsidRefusal::Listed) => bail!("excluded-ssids: \"{ssid}\" is listed twice"),
            }
        }
        Ok(())
    }
}

/// The most bytes an SSID holds, per 802.11.
pub const SSID_MAX_BYTES: usize = 32;

/// Why an SSID cannot join the excluded list. The file and the settings page refuse
/// the same entries, so the page never saves a list `load` would refuse; each words
/// the refusal its own way.
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
pub fn check_excluded_ssid(ssid: &str, listed: &[String]) -> std::result::Result<(), SsidRefusal> {
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

/// Load and validate `wartui.toml`.
///
/// `explicit` (`--config`) is read or the run fails: an operator who named a file
/// gets an error when it is not there, not a silent default. Without it, a missing
/// default file — or no `$HOME` to find one under — means [`Config::default`],
/// silently, since most operators have never written one.
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    load_from(explicit, default_path)
}

/// The bridge saved in `wartui.toml`, for a command that only reads it. `explicit` is
/// `--config`, resolved as [`load`] resolves it.
///
/// A file that is missing or does not load is no bridge remembered, logged at `debug`,
/// so a broken config never stops the command asking. That holds for a named file too:
/// [`load`]'s error for a missing `--config` file is none here. `remember = false` is
/// none either, since `load` refuses a `mac` beside it.
#[must_use]
pub fn remembered_bridge(explicit: Option<&Path>) -> Bridge {
    remembered_bridge_from(explicit, default_path)
}

/// [`remembered_bridge`], with where the default file lives supplied by the caller, as
/// [`load_from`] takes it.
fn remembered_bridge_from(
    explicit: Option<&Path>,
    default: impl FnOnce() -> Option<PathBuf>,
) -> Bridge {
    match load_from(explicit, default) {
        Ok(config) => config.bridge,
        Err(error) => {
            tracing::debug!(error = format_args!("{error:#}"), "no remembered bridge");
            Bridge::default()
        }
    }
}

/// Where `wartui.toml` would be read from or written to: `explicit`, falling back
/// to [`default_path`]. `load` resolves the same way; this is exposed so the view
/// can name a save's destination without loading the file itself.
#[must_use]
pub fn path(explicit: Option<&Path>) -> Option<PathBuf> {
    resolve(explicit, default_path)
}

fn resolve(explicit: Option<&Path>, default: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    explicit.map(Path::to_path_buf).or_else(default)
}

/// `load`, with where the default file lives supplied by the caller — a real lookup
/// in `load`, a fixed path in a test, so the "no `--config`, file missing" case is
/// exercised without mutating the process environment.
pub(crate) fn load_from(
    explicit: Option<&Path>,
    default: impl FnOnce() -> Option<PathBuf>,
) -> Result<Config> {
    let required = explicit.is_some();
    let Some(path) = resolve(explicit, default) else { return Ok(Config::default()) };

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let config: Config = match toml::from_str(&text) {
        Ok(config) => config,
        Err(error) => {
            let error = anyhow::Error::new(error);
            let error = match bridge_setting(&text) {
                Some(hint) => error.context(hint),
                None => error,
            };
            return Err(error.context(format!("parsing {}", path.display())));
        }
    };
    config.validate().with_context(|| format!("in {}", path.display()))?;
    Ok(config)
}

/// Why the file is refused, when it holds a setting the bridge keeps instead: the pool, and the
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
             add Bluetooth to the scan with `wartui scan start --bluetooth`, or `b` in `run`",
        )
    } else {
        None
    }
}

/// This host's `wartui.toml`: `XDG_CONFIG_HOME` when it is absolute, else `~/.config`, and
/// `~/Library/Application Support` on macOS.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    default_path_in(
        cfg!(target_os = "macos"),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    )
}

/// A pure function so the OS-dependent rules can be driven by a test without
/// touching the process environment.
fn default_path_in(macos: bool, home: Option<&OsStr>, xdg: Option<&OsStr>) -> Option<PathBuf> {
    if macos {
        let home = home?;
        return Some(Path::new(home).join("Library/Application Support/wartui/wartui.toml"));
    }
    if let Some(xdg) = xdg {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() {
            return Some(xdg.join("wartui/wartui.toml"));
        }
    }
    let home = home?;
    Some(Path::new(home).join(".config/wartui/wartui.toml"))
}

/// Write `config` to `wartui.toml` at `path`, replacing whatever the file held.
///
/// The file belongs to the view, which saves every setting the modal shows;
/// `flash-bridge` saves through here too, over the file it just loaded. Nothing on disk is read back or merged: a hand edit, a comment, or a file
/// that does not parse is simply overwritten. `config` is checked with
/// [`Config::validate`] first, so `save` never leaves a file `load` would refuse.
///
/// The write lands in a temp file and is renamed into place — a torn write here
/// would stop the next run from starting — beside the file `path` *resolves to*
/// rather than `path` itself, so a `path` that is a symlink stays one: the rename
/// replaces what it points at, not the link, even a dangling one. A file that
/// existed keeps its permissions, copied onto the temp file before the rename; a
/// new file is readable only by its owner on unix, since it can hold an API key.
/// Saving a key into an existing file also drops its group and other bits, so a
/// file written world-readable before it held a key stops being so. The temp file is
/// created fresh at `0600` on unix and loosened only after it is written, so the key is
/// never readable by others in passing.
///
/// Skipped when the file already holds exactly this text: no temp file and no
/// rename, though a key still has its permissions tightened in place.
pub fn save(path: &Path, config: &Config) -> Result<()> {
    config.validate().context("the settings to save are invalid")?;
    let written = toml::to_string(config).context("serializing the settings")?;

    let secret = !config.api_keys.wdgwars.is_empty();

    if std::fs::read_to_string(path).is_ok_and(|text| text == written) {
        // Same text, but a key in a file others can read still has to be shut away.
        if secret
            && let Ok(meta) = std::fs::metadata(path)
            && let Some(tight) = owner_only_if(meta.permissions(), secret)
        {
            std::fs::set_permissions(path, tight)
                .with_context(|| format!("setting permissions on {}", path.display()))?;
        }
        return Ok(());
    }

    // A path that does not exist yet has nothing to resolve or to have
    // permissions of, so it is used as given, and a new file gets the default.
    let target = resolve_symlink_target(path)?;
    let permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());

    if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut temp_name = target.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp_path = PathBuf::from(temp_name);
    match std::fs::remove_file(&temp_path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("removing {}", temp_path.display()));
        }
        _ => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&temp_path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, written.as_bytes()))
        .with_context(|| format!("writing {}", temp_path.display()))?;
    let permissions = match permissions {
        Some(existing) => Some(owner_only_if(existing.clone(), secret).unwrap_or(existing)),
        None => owner_only(),
    };
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&temp_path, permissions)
            .with_context(|| format!("setting permissions on {}", temp_path.display()))?;
    }
    std::fs::rename(&temp_path, &target).with_context(|| format!("saving {}", target.display()))?;
    Ok(())
}

/// Write a default `wartui.toml` at `path` when nothing is there, and say whether it
/// did. A dangling symlink counts as missing, and `save` creates its target.
pub fn create_if_missing(path: &Path) -> Result<bool> {
    if path.try_exists().with_context(|| format!("checking {}", path.display()))? {
        return Ok(false);
    }
    save(path, &Config::default())?;
    Ok(true)
}

/// Mode `0600` for a new file, on unix; elsewhere, whatever the platform gives it.
fn owner_only() -> Option<std::fs::Permissions> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// `permissions` without group and other bits when `secret` and either is set, or
/// `None` when nothing needs to change. Never anything on non-unix platforms.
fn owner_only_if(permissions: std::fs::Permissions, secret: bool) -> Option<std::fs::Permissions> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode();
        (secret && mode & 0o077 != 0).then(|| std::fs::Permissions::from_mode(mode & !0o077))
    }
    #[cfg(not(unix))]
    {
        let _ = (permissions, secret);
        None
    }
}

/// Follow `path` through however many symlinks it is, to the file a write
/// through it ultimately lands on — even one that does not exist yet, which
/// is where [`std::fs::canonicalize`] falls short: it refuses a dangling
/// link, and the fallback of using `path` itself would make `save` replace
/// the link with a plain file instead of writing through it. A relative link
/// target is resolved against the link's own parent directory, the same way
/// a shell would. Bounded at 40 hops so a cycle errors rather than spinning.
fn resolve_symlink_target(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let Ok(metadata) = std::fs::symlink_metadata(&current) else { return Ok(current) };
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)
            .with_context(|| format!("reading link {}", current.display()))?;
        current = if link.is_absolute() {
            link
        } else {
            current.parent().unwrap_or_else(|| Path::new("")).join(link)
        };
    }
    bail!("too many levels of symbolic links: {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::{
        ApiKeys, Bridge, Config, Gps, create_if_missing, default_path_in, excluded_ssid_bytes,
        load, load_from, path, remembered_bridge_from, save,
    };
    use std::ffi::OsStr;

    #[test]
    fn config_path_uses_absolute_xdg_config_home_when_set() {
        let path = default_path_in(false, Some(OsStr::new("/home/op")), Some(OsStr::new("/x")));
        assert_eq!(path, Some("/x/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_path_ignores_relative_xdg_config_home_when_set() {
        let path =
            default_path_in(false, Some(OsStr::new("/home/op")), Some(OsStr::new("relative")));
        assert_eq!(path, Some("/home/op/.config/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_path_falls_back_to_dot_config_when_xdg_unset() {
        let path = default_path_in(false, Some(OsStr::new("/home/op")), None);
        assert_eq!(path, Some("/home/op/.config/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_path_uses_application_support_when_macos() {
        let path = default_path_in(true, Some(OsStr::new("/Users/op")), Some(OsStr::new("/x")));
        assert_eq!(path, Some("/Users/op/Library/Application Support/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_path_returns_none_when_home_unset() {
        assert_eq!(default_path_in(false, None, None), None);
        assert_eq!(default_path_in(true, None, None), None);
    }

    #[test]
    fn config_loader_returns_default_when_file_missing_and_no_path_given() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("wartui.toml");
        let config = load_from(None, || Some(missing)).unwrap();
        assert!(config.gps.bridge_rate_hz.is_none());
    }

    #[test]
    fn config_loader_returns_default_when_no_default_path_available() {
        // No `$HOME`, so `default_path()` returns `None` and there is nowhere to
        // look — the same as a missing file, but with the lookup itself absent.
        let config = load_from(None, || None).unwrap();
        assert!(config.excluded_ssids.is_empty());
    }

    #[test]
    fn config_loader_errors_when_explicit_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let error = load(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("missing.toml"), "{error}");
    }

    #[test]
    fn config_loader_points_at_pool_command_when_file_has_pool_key() {
        // The bridge holds the pool, so the file has no key for it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "pool = \"us\"\n").unwrap();
        let chain = format!("{:#}", load(Some(&path)).unwrap_err());
        assert!(chain.contains("wartui.toml"), "{chain}");
        assert!(chain.contains("set it with `wartui pool`"), "{chain}");
        assert!(chain.contains("line 1"), "the parse error still names the line: {chain}");
    }

    #[test]
    fn config_loader_accepts_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "").unwrap();
        let config: Config = load(Some(&path)).unwrap();
        assert!(config.excluded_ssids.is_empty());
    }

    #[test]
    fn config_loader_rejects_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nremember = true\ntypo = 1\n").unwrap();
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn config_loader_rejects_unknown_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[not-a-real-table]\nx = 1\n").unwrap();
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn config_loader_reads_bridge_rate_when_gps_table_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[gps]\nbridge-rate-hz = 2\n").unwrap();
        assert_eq!(load(Some(&path)).unwrap().gps.bridge_rate_hz, Some(2));
    }

    #[test]
    fn config_loader_rejects_bridge_rate_when_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        for rate in [0, 6] {
            std::fs::write(&path, format!("[gps]\nbridge-rate-hz = {rate}\n")).unwrap();
            let chain = format!("{:#}", load(Some(&path)).unwrap_err());
            assert!(chain.contains("gps.bridge-rate-hz"), "{chain}");
        }
    }

    #[test]
    fn config_loader_rejects_unknown_key_when_under_gps_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[gps]\nrate = 2\n").unwrap();
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn config_save_omits_gps_table_when_rate_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        save(&target, &Config::default()).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(!text.contains("[gps]"), "{text}");
    }

    #[test]
    fn config_save_writes_gps_table_that_reloads_when_rate_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        save(&target, &full()).unwrap();
        assert_eq!(load(Some(&target)).unwrap().gps.bridge_rate_hz, Some(2));
    }

    #[test]
    fn config_loader_names_file_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[gps]\nbridge-rate-hz = 9\n").unwrap();
        let error = load(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("wartui.toml"), "{error}");
    }

    #[test]
    fn config_loader_points_at_scan_command_when_file_has_bluetooth_table() {
        // The bridge picks the node that scans Bluetooth, so the table is as unknown as any other.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bluetooth]\nremember = true\n").unwrap();
        let chain = format!("{:#}", load(Some(&path)).unwrap_err());
        assert!(chain.contains("wartui.toml"), "{chain}");
        assert!(chain.contains("`wartui scan start --bluetooth`"), "{chain}");
    }

    #[test]
    fn config_loader_adds_no_bridge_hint_when_unknown_table_is_another() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[not-a-real-table]\nx = 1\n").unwrap();
        let chain = format!("{:#}", load(Some(&path)).unwrap_err());
        assert!(!chain.contains("the bridge"), "{chain}");
    }

    #[test]
    fn config_path_returns_explicit_when_given() {
        let explicit = std::path::PathBuf::from("/custom/wartui.toml");
        assert_eq!(path(Some(&explicit)), Some(explicit));
    }

    /// A `Config` holding every key the settings modal saves.
    fn full() -> Config {
        Config {
            excluded_ssids: vec!["HomeNet".to_owned(), "Pixel 9".to_owned()],
            bridge: Bridge {
                remember: Some(true),
                mac: Some([0xAA, 0xBB, 0xCC, 1, 2, 3]),
                serial: Some("5CF7153520".to_owned()),
            },
            gps: Gps { bridge_rate_hz: Some(2) },
            api_keys: ApiKeys { wdgwars: "abc123def456".to_owned() },
        }
    }

    /// A `Config` with only the bridge's GPS rate set.
    fn rate_only(rate: u8) -> Config {
        Config {
            excluded_ssids: Vec::new(),
            bridge: Bridge::default(),
            gps: Gps { bridge_rate_hz: Some(rate) },
            api_keys: ApiKeys::default(),
        }
    }

    #[test]
    fn config_save_creates_file_and_parent_directories_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/deeper/wartui.toml");

        save(&target, &full()).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(2));
    }

    #[test]
    fn config_save_writes_every_key_that_reloads_when_all_are_set() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &full()).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(2));
        assert_eq!(saved.excluded_ssids, ["HomeNet", "Pixel 9"]);
        assert_eq!(saved.api_keys.wdgwars, "abc123def456");
    }

    #[test]
    fn config_save_writes_excluded_ssids_that_reload() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &full()).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.excluded_ssids, ["HomeNet", "Pixel 9"]);
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("excluded-ssids = [\"HomeNet\", \"Pixel 9\"]"), "{text}");
    }

    #[test]
    fn config_excluded_ssid_bytes_yields_each_entry_as_bytes_when_list_is_set() {
        let config = full();
        let bytes: Vec<Vec<u8>> = excluded_ssid_bytes(&config.excluded_ssids).collect();
        assert_eq!(bytes, [b"HomeNet".to_vec(), b"Pixel 9".to_vec()]);
    }

    #[test]
    fn config_save_omits_excluded_ssids_when_list_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &rate_only(3)).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(!text.contains("excluded-ssids"), "{text}");
    }

    /// The error chain from loading a file holding `text`.
    fn load_error(text: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, text).unwrap();
        format!("{:#}", load(Some(&path)).unwrap_err())
    }

    #[test]
    fn config_loader_rejects_excluded_ssid_when_entry_is_empty() {
        let chain = load_error("excluded-ssids = [\"HomeNet\", \"\"]\n");
        assert!(chain.contains("excluded-ssids: an entry is empty"), "{chain}");
    }

    #[test]
    fn config_loader_rejects_excluded_ssid_when_entry_is_over_32_bytes() {
        let chain = load_error(&format!("excluded-ssids = [\"{}\"]\n", "x".repeat(33)));
        assert!(chain.contains("longer than 32 bytes"), "{chain}");
    }

    #[test]
    fn config_loader_rejects_excluded_ssid_when_entry_is_listed_twice() {
        let chain = load_error("excluded-ssids = [\"HomeNet\", \"Pixel 9\", \"HomeNet\"]\n");
        assert!(chain.contains("\"HomeNet\" is listed twice"), "{chain}");
    }

    #[test]
    fn config_save_omits_bridge_table_when_remember_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &rate_only(3)).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(!text.contains("[bridge]"), "{text}");
    }

    #[test]
    fn config_save_writes_bridge_table_that_reloads_when_remember_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let config = Config {
            bridge: Bridge { remember: Some(false), ..Bridge::default() },
            ..Config::default()
        };

        save(&target, &config).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("[bridge]\nremember = false"), "{text}");
        assert_eq!(load(Some(&target)).unwrap().bridge.remember, Some(false));
    }

    #[test]
    fn config_loader_rejects_unknown_key_when_under_bridge_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nremember = true\nport = \"/dev/ttyACM0\"\n").unwrap();
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn config_loader_reads_bridge_table_when_mac_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nmac = \"aa:BB:cc:01:02:03\"\n").unwrap();
        let config = load(Some(&path)).unwrap();
        assert_eq!(config.bridge.remember, None);
        assert_eq!(config.bridge.mac, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
    }

    #[test]
    fn config_save_writes_bridge_table_that_reloads_when_mac_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &full()).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.bridge.remember, Some(true));
        assert_eq!(saved.bridge.mac, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            text.contains("[bridge]\nremember = true\nmac = \"AA:BB:CC:01:02:03\""),
            "as `wartui ports` prints it: {text}"
        );
        assert_eq!(saved.bridge.serial.as_deref(), Some("5CF7153520"));
    }

    #[test]
    fn config_loader_rejects_bridge_mac_when_malformed() {
        let chain = load_error("[bridge]\nmac = \"AA:BB:CC\"\n");
        assert!(chain.contains("bridge.mac"), "{chain}");
    }

    #[test]
    fn config_loader_rejects_bridge_mac_when_remember_is_off() {
        let chain = load_error("[bridge]\nremember = false\nmac = \"AA:BB:CC:01:02:03\"\n");
        assert!(chain.contains("bridge.mac"), "{chain}");
    }

    #[test]
    fn config_remembered_bridge_returns_mac_when_file_holds_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\n").unwrap();
        assert_eq!(
            remembered_bridge_from(None, || Some(path)).mac,
            Some([0xAA, 0xBB, 0xCC, 1, 2, 3])
        );
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("wartui.toml");
        assert_eq!(remembered_bridge_from(None, || Some(missing)).mac, None);
        assert_eq!(remembered_bridge_from(None, || None).mac, None);
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_file_does_not_load() {
        // A broken config must not stop a command that only reads the bridge.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        for text in ["this is [not toml\n", "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\ntypo = 1\n"] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(remembered_bridge_from(None, || Some(path.clone())).mac, None, "{text:?}");
        }
    }

    #[test]
    fn config_remembered_bridge_reads_explicit_file_when_one_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("named.toml");
        let default = dir.path().join("wartui.toml");
        std::fs::write(&named, "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\n").unwrap();
        std::fs::write(&default, "[bridge]\nmac = \"10:BD:A3:EC:44:C0\"\n").unwrap();
        let found = remembered_bridge_from(Some(&named), || Some(default)).mac;
        assert_eq!(found, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_explicit_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        assert_eq!(remembered_bridge_from(Some(&missing), || None).mac, None);
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_remember_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nremember = false\n").unwrap();
        assert_eq!(remembered_bridge_from(None, || Some(path)).mac, None);
    }

    #[test]
    fn config_remembered_bridge_returns_serial_when_file_holds_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        let text = "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\nserial = \"5CF7153520\"\n";
        std::fs::write(&path, text).unwrap();
        let bridge = remembered_bridge_from(None, || Some(path));
        assert_eq!(bridge.serial.as_deref(), Some("5CF7153520"));
    }

    #[test]
    fn config_loader_rejects_bridge_serial_when_remember_is_off() {
        let chain = load_error("[bridge]\nremember = false\nserial = \"5CF7153520\"\n");
        assert!(chain.contains("bridge.serial"), "{chain}");
    }

    #[test]
    fn config_save_refuses_out_of_range_value_and_leaves_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let original = "[gps]\nbridge-rate-hz = 1\n";
        std::fs::write(&target, original).unwrap();

        let result = save(&target, &rate_only(9));

        assert!(result.is_err());
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, original, "a refused write must not touch the file");
    }

    #[test]
    fn config_save_replaces_everything_when_file_holds_other_values() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(
            &target,
            "# notes\nexcluded-ssids = [\"HomeNet\"]\n[bridge]\nremember = false\n",
        )
        .unwrap();

        save(&target, &rate_only(3)).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert!(saved.excluded_ssids.is_empty(), "the file holds only what was saved");
        assert_eq!(saved.bridge.remember, None);
        assert_eq!(saved.gps.bridge_rate_hz, Some(3));
    }

    #[test]
    fn config_save_overwrites_file_when_existing_file_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "this is [not toml\n").unwrap();

        save(&target, &full()).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.excluded_ssids, ["HomeNet", "Pixel 9"]);
    }

    #[test]
    #[cfg(unix)]
    fn config_save_writes_through_symlink_when_path_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::fs::write(&real, "[gps]\nbridge-rate-hz = 1\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        save(&link, &rate_only(3)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        let saved = load(Some(&real)).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(3), "the file it pointed to got the change");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_creates_target_when_path_is_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap(); // target does not exist yet

        save(&link, &rate_only(3)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        assert_eq!(std::fs::read_link(&link).unwrap(), target, "still points at the same place");
        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(3), "the link's target got created");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_follows_a_relative_chain_of_dangling_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let middle = dir.path().join("middle.toml");
        let link = dir.path().join("wartui.toml");
        // `middle` names `real.toml` relative to its own directory, and neither
        // it nor `real.toml` exists yet.
        std::os::unix::fs::symlink("real.toml", &middle).unwrap();
        std::os::unix::fs::symlink(&middle, &link).unwrap();

        save(&link, &rate_only(4)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(std::fs::symlink_metadata(&middle).unwrap().file_type().is_symlink());
        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(4), "the chain's final target got created");
    }

    #[test]
    fn config_save_leaves_file_untouched_when_it_already_holds_the_same_text() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        save(&target, &full()).unwrap();
        let before = std::fs::metadata(&target).unwrap().modified().unwrap();

        save(&target, &full()).unwrap();

        let after = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(before, after, "identical text is never rewritten");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_keeps_permissions_when_file_exists() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[gps]\nbridge-rate-hz = 1\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        save(&target, &rate_only(3)).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the mode survives the rewrite");
    }

    #[test]
    fn config_save_writes_empty_api_key_when_config_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &Config::default()).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, "[api-keys]\nwdgwars = \"\"\n");
    }

    #[test]
    fn config_loader_reads_api_keys_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[api-keys]\nwdgwars = \"abc\"\n").unwrap();
        assert_eq!(load(Some(&path)).unwrap().api_keys.wdgwars, "abc");
    }

    #[test]
    fn config_loader_returns_empty_api_key_when_table_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[gps]\nbridge-rate-hz = 2\n").unwrap();
        assert_eq!(load(Some(&path)).unwrap().api_keys.wdgwars, "");
    }

    #[test]
    fn config_loader_rejects_unknown_key_when_under_api_keys_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[api-keys]\nwigle = \"abc\"\n").unwrap();
        assert!(load(Some(&path)).is_err());
    }

    #[test]
    fn config_save_writes_api_key_that_reloads_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &full()).unwrap();

        assert_eq!(load(Some(&target)).unwrap().api_keys.wdgwars, "abc123def456");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_creates_owner_only_file_when_file_is_new() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &Config::default()).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the file can hold an API key");
    }

    #[test]
    fn config_create_if_missing_writes_default_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/wartui.toml");

        assert!(create_if_missing(&target).unwrap());

        let config = load(Some(&target)).unwrap();
        assert_eq!(config.api_keys.wdgwars, "");
        assert!(config.excluded_ssids.is_empty());
        assert!(config.gps.bridge_rate_hz.is_none());
    }

    #[test]
    fn config_create_if_missing_leaves_file_untouched_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let original = "# mine\n[gps]\nbridge-rate-hz = 1\n";
        std::fs::write(&target, original).unwrap();

        assert!(!create_if_missing(&target).unwrap());

        assert_eq!(std::fs::read_to_string(&target).unwrap(), original);
    }

    #[test]
    #[cfg(unix)]
    fn config_create_if_missing_creates_target_when_path_is_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(create_if_missing(&link).unwrap());

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        assert_eq!(load(Some(&target)).unwrap().api_keys.wdgwars, "");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_drops_group_and_other_bits_when_key_saved_into_open_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[gps]\nbridge-rate-hz = 1\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

        save(&target, &full()).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a key is readable only by its owner");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_drops_group_and_other_bits_when_text_unchanged_and_key_set() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        save(&target, &full()).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

        save(&target, &full()).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the skipped write still tightens the mode");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_keeps_open_mode_when_key_is_empty() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[gps]\nbridge-rate-hz = 1\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();

        save(&target, &rate_only(3)).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "no key, nothing to hide");
    }

    #[test]
    #[cfg(unix)]
    fn config_save_replaces_stale_temp_file_when_one_is_left_over() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let stale = dir.path().join("wartui.toml.tmp");
        std::fs::write(&stale, "junk from an earlier save").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();

        save(&target, &full()).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the stale file's mode is not inherited");
        assert!(!stale.exists(), "no temp file is left behind");
        assert_eq!(load(Some(&target)).unwrap().api_keys.wdgwars, "abc123def456");
    }
}
