//! `wartui.toml` — settings an operator wants to stop typing every run. The schema and its rules
//! are `wartui_app::settings`'s, and the file itself, where it lives and how a save replaces it,
//! is `wartui_desktop::config`'s. This is which commands read and write it.
//!
//! **The operator's own decision**, so a mistake in it is refused rather than absorbed:
//! an unknown key or an out-of-range value stops `run`, `export`, `analyze`, `upload`
//! and `flash-bridge` from starting, and names the file and the problem.
//! `--config` is one of each's own arguments. Within `run`, the file beats the built-in
//! default. `export`, `analyze` and `upload` read the excluded SSIDs, `upload` the API keys
//! too, and none of them creates or saves the file.
//!
//! **The remembered bridge is the one value other commands read.** `status`, `sniff`,
//! `reset`, `power-cycle`, `fan`, `pool`, `scan`, `ota` and `bluetooth` take `[bridge] mac`,
//! `serial` and `bluetooth-address` through [`remembered_bridge`] to pick or order boards, and all
//! but `bluetooth pair` take `link` to pick the link, and a missing or broken file is no bridge
//! remembered there, so a bad `wartui.toml` cannot stop a command that checks the link.
//! `flash-bridge` takes it through [`load`] and refuses a broken file: which board it writes
//! firmware over depends on it. It takes `link` as the others do. `flash-fleet` never reads the
//! file. Neither `bluetooth pair` nor `bluetooth forget` writes it.
//!
//! **Two commands write the bridge.** `run` creates the file once when it is absent,
//! holding only the empty API keys, so a first-time operator has a place to paste them. It
//! never rewrites an existing file at startup. Every later write goes through one save path
//! (`wartui_app::settings::Settings`). The view saves the settings modal and the box on the
//! waiting-for-GPS pop-up, which writes `[gps] wait-for-fix`. The run's session
//! (`wartui_app::session`) saves a change to the bridge the transport remembers, and the link the
//! run connected over, which writes `[bridge] link`.
//! `flash-bridge` saves the board it found and flashed as the bridge, unless remembering
//! is off, so `run` and the next `flash-bridge` open the new bridge first. A board named
//! with `--bridge` is often some other device, and is not saved.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::builder::{EnumValueParser, PossibleValue, TypedValueParser};
use wartui_app::settings::SettingsStore;
use wartui_desktop::config::ConfigFile;

pub use wartui_app::settings::{
    Bridge, Config, Link, SsidRefusal, check_excluded_ssid, excluded_ssid_bytes,
};

/// The `--link` flag, flattened into every command that reaches the bridge.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct LinkFlag {
    /// Which link reaches the bridge. `[bridge] link` in `wartui.toml` when omitted, else
    /// `usb`. A `--bridge` path is always USB.
    #[arg(long, value_name = "LINK", value_parser = link_parser())]
    pub link: Option<Link>,
}

/// `--link`'s values, each with its own help.
#[derive(Debug, Clone, Copy)]
struct LinkValue(Link);

impl clap::ValueEnum for LinkValue {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self(Link::Usb), Self(Link::Bluetooth)]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(match self.0 {
            Link::Usb => PossibleValue::new("usb").help("The board's CH343, over USB"),
            Link::Bluetooth => PossibleValue::new("bluetooth")
                .help("The board's C6, over Bluetooth, to a PC paired with it"),
        })
    }
}

/// The parser of a flag naming a [`Link`].
pub fn link_parser() -> impl TypedValueParser<Value = Link> {
    EnumValueParser::<LinkValue>::new().map(|LinkValue(link)| link)
}

/// Load and validate `wartui.toml`.
///
/// `explicit` (`--config`) is read or the run fails: an operator who named a file
/// gets an error when it is not there, not a silent default. Without it, a missing
/// default file — or no `$HOME` to find one under — means [`Config::default`],
/// silently, since most operators have never written one.
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    load_file(ConfigFile::new(explicit).as_ref())
}

/// The settings `file` holds, or [`Config::default`] with no file at all.
fn load_file(file: Option<&ConfigFile>) -> Result<Config> {
    let Some(file) = file else { return Ok(Config::default()) };
    Ok(wartui_app::settings::load(file)?)
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
    remembered_bridge_in(ConfigFile::new(explicit).as_ref())
}

/// [`remembered_bridge`] from `file`.
fn remembered_bridge_in(file: Option<&ConfigFile>) -> Bridge {
    match load_file(file) {
        Ok(config) => config.bridge,
        Err(error) => {
            tracing::debug!(error = format_args!("{error:#}"), "no remembered bridge");
            Bridge::default()
        }
    }
}

/// Where `wartui.toml` would be read from or written to: `explicit`, falling back
/// to the default file. `load` resolves the same way; this is exposed so a command
/// can name a save's destination without loading the file itself.
#[must_use]
pub fn path(explicit: Option<&Path>) -> Option<PathBuf> {
    ConfigFile::new(explicit).map(|file| file.path().to_path_buf())
}

/// The settings in the `wartui.toml` at `path`, where a missing file is [`Config::default`].
pub fn load_at(path: &Path) -> Result<Config> {
    load_file(Some(&ConfigFile::at(path.to_path_buf())))
}

/// Write `config` to `wartui.toml` at `path`, replacing whatever the file held.
///
/// `config` is checked first, so a save never leaves a file `load` would refuse; how the
/// write lands is `wartui_desktop::config::ConfigFile`'s.
pub fn save(path: &Path, config: &Config) -> Result<()> {
    let text = wartui_app::settings::serialize(config)?;
    ConfigFile::at(path.to_path_buf()).save(&text, config.api_keys.any_set())?;
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use wartui_app::settings::Gps;
    use wartui_desktop::config::ConfigFile;

    use super::{Config, create_if_missing, load, load_file, path, remembered_bridge_in, save};

    /// [`super::remembered_bridge`], with the default file at `default`.
    fn remembered(explicit: Option<&Path>, default: Option<PathBuf>) -> super::Bridge {
        remembered_bridge_in(ConfigFile::resolve(explicit, || default).as_ref())
    }

    #[test]
    fn config_load_returns_default_when_no_default_path_available() {
        // No `$HOME`, so there is no default file and nowhere to look — the same as a missing
        // file, but with the lookup itself absent.
        let config = load_file(ConfigFile::resolve(None, || None).as_ref()).unwrap();
        assert!(config.excluded_ssids.is_empty());
    }

    #[test]
    fn config_load_points_at_pool_command_when_file_has_pool_key() {
        // The chain an operator reads, through anyhow as every command prints it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "pool = \"us\"\n").unwrap();
        let chain = format!("{:#}", load(Some(&path)).unwrap_err());
        assert!(chain.starts_with("parsing "), "{chain}");
        assert!(chain.contains("wartui.toml: the bridge holds the channel pool"), "{chain}");
        assert!(chain.contains("set it with `wartui pool`"), "{chain}");
        assert!(chain.contains("line 1"), "the parse error still names the line: {chain}");
    }

    /// A command taking only `--link`.
    #[derive(clap::Parser)]
    struct Flagged {
        #[command(flatten)]
        link_flag: super::LinkFlag,
    }

    #[test]
    fn config_link_flag_reads_each_link_when_given() {
        use clap::Parser;
        for (text, link) in [("usb", super::Link::Usb), ("bluetooth", super::Link::Bluetooth)] {
            let parsed = Flagged::try_parse_from(["wartui", "--link", text]).unwrap();
            assert_eq!(parsed.link_flag.link, Some(link));
        }
        assert!(Flagged::try_parse_from(["wartui", "--link", "wifi"]).is_err());
    }

    #[test]
    fn config_path_returns_explicit_when_given() {
        let explicit = std::path::PathBuf::from("/custom/wartui.toml");
        assert_eq!(path(Some(&explicit)), Some(explicit));
    }

    #[test]
    fn config_remembered_bridge_returns_mac_when_file_holds_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\n").unwrap();
        assert_eq!(remembered(None, Some(path)).mac, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("wartui.toml");
        assert_eq!(remembered(None, Some(missing)).mac, None);
        assert_eq!(remembered(None, None).mac, None);
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_file_does_not_load() {
        // A broken config must not stop a command that only reads the bridge.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        for text in ["this is [not toml\n", "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\ntypo = 1\n"] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(remembered(None, Some(path.clone())).mac, None, "{text:?}");
        }
    }

    #[test]
    fn config_remembered_bridge_reads_explicit_file_when_one_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("named.toml");
        let default = dir.path().join("wartui.toml");
        std::fs::write(&named, "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\n").unwrap();
        std::fs::write(&default, "[bridge]\nmac = \"02:00:5E:10:44:C0\"\n").unwrap();
        let found = remembered(Some(&named), Some(default)).mac;
        assert_eq!(found, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_explicit_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        assert_eq!(remembered(Some(&missing), None).mac, None);
    }

    #[test]
    fn config_remembered_bridge_returns_none_when_remember_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "[bridge]\nremember = false\n").unwrap();
        assert_eq!(remembered(None, Some(path)).mac, None);
    }

    #[test]
    fn config_remembered_bridge_returns_serial_when_file_holds_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        let text = "[bridge]\nmac = \"AA:BB:CC:01:02:03\"\nserial = \"AAAAAAAAAA\"\n";
        std::fs::write(&path, text).unwrap();
        let bridge = remembered(None, Some(path));
        assert_eq!(bridge.serial.as_deref(), Some("AAAAAAAAAA"));
    }

    #[test]
    fn config_save_refuses_out_of_range_value_and_leaves_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let original = "[gps]\nbridge-rate-hz = 1\n";
        std::fs::write(&target, original).unwrap();
        let invalid = Config {
            gps: Gps { bridge_rate_hz: Some(9), wait_for_fix: None },
            ..Config::default()
        };

        let result = save(&target, &invalid);

        let chain = format!("{:#}", result.unwrap_err());
        assert!(chain.starts_with("the settings to save are invalid: "), "{chain}");
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, original, "a refused write must not touch the file");
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
}
