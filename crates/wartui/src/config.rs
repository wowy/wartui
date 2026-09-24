//! `wartui.toml` — settings an operator wants to stop typing every run.
//!
//! **Configuration, not state.** This is the operator's own decision, so a mistake in
//! it is refused rather than absorbed: an unknown key or an out-of-range value stops
//! wartui from starting and names the file and the problem, the same way an
//! out-of-range `--node-tx-power` does. `crates/wartui-bridge/src/remember.rs` is the
//! opposite case — a cache thrown away and rebuilt when it is wrong — and the
//! distinction is why that file is silent about failures and this one is not.
//!
//! Only the commands that read a setting from here load the file, so a broken
//! `wartui.toml` cannot stop `ports`, `status` or `reset` from working; `main.rs`
//! loads it once, for `run` alone. Within `run`, the command line beats the file and
//! the file beats the default: a flag typed for this one invocation is a more recent
//! decision than a file left on disk, and both outrank silently defaulting to 2 dBm.
//!
//! This lives in `crates/wartui` rather than `wartui-core`: the core crate parses no
//! arguments, and a config file is operator input just like a flag is.

use std::ffi::OsStr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Everything `wartui.toml` can hold. Add a field and a table to grow it; every
/// struct denies unknown fields, so a typo in the file is caught rather than
/// silently ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[serde(default)]
    pub tx_power: TxPower,
}

/// The `[tx-power]` table: `fleet` covers the nodes and `bridge` the bridge, each
/// independent and falling back to the default on its own — the same split
/// `--node-tx-power` and `--bridge-tx-power` make on the command line.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TxPower {
    pub fleet: Option<i8>,
    pub bridge: Option<i8>,
}

/// Whole dBm a transmit power may name, on the command line or in the file — 20 dBm
/// is the ceiling because whether anything above it works correctly is unverified,
/// per `AGENTS.md`'s tx-power invariant.
pub const TX_POWER_DBM: RangeInclusive<i8> = 2..=20;

impl Config {
    fn validate(&self) -> Result<()> {
        check_power("tx-power.fleet", self.tx_power.fleet)?;
        check_power("tx-power.bridge", self.tx_power.bridge)?;
        Ok(())
    }
}

fn check_power(key: &str, value: Option<i8>) -> Result<()> {
    if let Some(value) = value
        && !TX_POWER_DBM.contains(&value)
    {
        bail!("{key} = {value} is outside {} to {} dBm", TX_POWER_DBM.start(), TX_POWER_DBM.end());
    }
    Ok(())
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

/// `load`, with where the default file lives supplied by the caller — a real lookup
/// in `load`, a fixed path in a test, so the "no `--config`, file missing" case is
/// exercised without mutating the process environment.
fn load_from(explicit: Option<&Path>, default: impl FnOnce() -> Option<PathBuf>) -> Result<Config> {
    let (path, required) = match explicit {
        Some(path) => (Some(path.to_path_buf()), true),
        None => (default(), false),
    };
    let Some(path) = path else { return Ok(Config::default()) };

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    config.validate().with_context(|| format!("in {}", path.display()))?;
    Ok(config)
}

/// This host's `wartui.toml`, following the same per-OS rules
/// `crates/wartui-bridge/src/remember.rs`'s `state_dir` uses for its own directory.
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

#[cfg(test)]
mod tests {
    use super::{Config, default_path_in, load, load_from};
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
        assert!(config.tx_power.fleet.is_none());
        assert!(config.tx_power.bridge.is_none());
    }

    #[test]
    fn config_loader_returns_default_when_no_default_path_available() {
        // No `$HOME`, so `default_path()` returns `None` and there is nowhere to
        // look — the same as a missing file, but with the lookup itself absent.
        let config = load_from(None, || None).unwrap();
        assert!(config.tx_power.fleet.is_none());
    }

    #[test]
    fn config_loader_errors_when_explicit_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let error = load(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("missing.toml"), "{error}");
    }

    #[test]
    fn config_loader_reads_tx_power_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[tx-power]\nfleet = 10\nbridge = 15\n").unwrap();
        let config = load(Some(&path)).unwrap();
        assert_eq!(config.tx_power.fleet, Some(10));
        assert_eq!(config.tx_power.bridge, Some(15));
    }

    #[test]
    fn config_loader_accepts_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "").unwrap();
        let config: Config = load(Some(&path)).unwrap();
        assert!(config.tx_power.fleet.is_none());
    }

    #[test]
    fn config_loader_rejects_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[tx-power]\nfleet = 10\ntypo = 1\n").unwrap();
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
    fn config_loader_rejects_fleet_power_above_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[tx-power]\nfleet = 25\n").unwrap();
        let error = load(Some(&path)).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("tx-power.fleet"), "{chain}");
    }

    #[test]
    fn config_loader_rejects_bridge_power_below_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[tx-power]\nbridge = 1\n").unwrap();
        let error = load(Some(&path)).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("tx-power.bridge"), "{chain}");
    }

    #[test]
    fn config_loader_names_file_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[tx-power]\nfleet = 25\n").unwrap();
        let error = load(Some(&path)).unwrap_err();
        assert!(error.to_string().contains("wartui.toml"), "{error}");
    }
}
