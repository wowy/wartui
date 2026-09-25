//! `wartui.toml` — settings an operator wants to stop typing every run.
//!
//! **Configuration, not state.** This is the operator's own decision, so a mistake in
//! it is refused rather than absorbed: an unknown key or an out-of-range value stops
//! wartui from starting and names the file and the problem, the same way an
//! out-of-range `--node-tx-power` does. `crates/wartui-bridge/src/remember.rs` is the
//! opposite case — a cache thrown away and rebuilt when it is wrong — and the
//! distinction is why that file is silent about failures and this one is not.
//!
//! Only `run` loads the file — `--config` is one of its own arguments — so a broken
//! `wartui.toml` cannot stop `ports`, `status` or `reset` from working. Within `run`,
//! the command line beats the file and the file beats the default: a flag typed for
//! this one invocation is a more recent decision than a file left on disk, and both
//! outrank silently defaulting to 2 dBm.
//!
//! This lives in `crates/wartui` rather than `wartui-core`: the core crate parses no
//! arguments, and a config file is operator input just like a flag is.

use std::ffi::OsStr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml_edit::DocumentMut;

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
#[derive(Debug, Default, Clone, Deserialize)]
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
fn load_from(explicit: Option<&Path>, default: impl FnOnce() -> Option<PathBuf>) -> Result<Config> {
    let required = explicit.is_some();
    let Some(path) = resolve(explicit, default) else { return Ok(Config::default()) };

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

/// Change `wartui.toml` at `path`, applying `change` to whatever is on disk right
/// now rather than to a value remembered from an earlier `load`.
///
/// `change` sees the file's *current* `Config`, not a value built from scratch: a
/// field it leaves alone keeps whatever the file already held, so a future
/// setting beside `tx-power` is not wiped out by a save that only meant to touch
/// this one. The file is parsed fresh even to get there, and a parse error is
/// propagated rather than swallowed — a file `load` would already refuse cannot
/// be safely updated, since there is no trustworthy `Config` to hand `change`.
///
/// The result is written into a [`DocumentMut`] read from the same text, so a
/// comment, a key's position and anything else the operator wrote by hand
/// survive a change of keys this doesn't touch: a `Some` field is set, a `None`
/// field is removed, and a table left with nothing under it is dropped rather
/// than kept as an empty `[tx-power]`. Re-parsed through [`Config::validate`]
/// before anything is written, so `update` never leaves a file `load` would
/// refuse either.
///
/// The write itself lands in a temp file and is renamed into place — a torn
/// write here would stop the next run from starting — beside the file `path`
/// *resolves to* rather than `path` itself, so a `path` that is a symlink stays
/// one: the rename replaces what it points at, not the link, even a dangling
/// one. A file that existed keeps its permissions, copied onto the temp file
/// before the rename; a new file gets whatever the process umask gives it.
///
/// Skipped entirely when `change` leaves the document identical to what was
/// read: no temp file, no rename, and — for a file that did not exist —
/// nothing created. This is what lets a save whose value turns out to match
/// the file exactly report success without disturbing it.
pub fn update(path: &Path, change: impl FnOnce(&mut Config)) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: DocumentMut =
        text.parse().with_context(|| format!("parsing {}", path.display()))?;
    let mut config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    change(&mut config);
    write_tx_power(&mut doc, &config.tx_power);

    let written = doc.to_string();
    let reparsed: Config =
        toml::from_str(&written).context("the settings just written do not parse")?;
    reparsed.validate().context("the settings just written are invalid")?;

    if written == text {
        return Ok(());
    }

    // A path that does not exist yet has nothing to resolve or to have
    // permissions of, so it is used as given and a new file gets the default.
    let target = resolve_symlink_target(path)?;
    let permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());

    if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut temp_name = target.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp_path = PathBuf::from(temp_name);
    std::fs::write(&temp_path, &written)
        .with_context(|| format!("writing {}", temp_path.display()))?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&temp_path, permissions)
            .with_context(|| format!("setting permissions on {}", temp_path.display()))?;
    }
    std::fs::rename(&temp_path, &target).with_context(|| format!("saving {}", target.display()))?;
    Ok(())
}

/// Follow `path` through however many symlinks it is, to the file a write
/// through it ultimately lands on — even one that does not exist yet, which
/// is where [`std::fs::canonicalize`] falls short: it refuses a dangling
/// link, and the fallback of using `path` itself would make `update` replace
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

/// Set or remove `[tx-power]`'s two keys, dropping the table entirely once
/// both are gone. `as_table_like` rather than `as_table` throughout, so a file
/// that wrote `tx-power = { fleet = 10 }` as an inline table is edited in place
/// rather than silently left alone.
fn write_tx_power(doc: &mut DocumentMut, tx_power: &TxPower) {
    set_field(doc, "tx-power", "fleet", tx_power.fleet);
    set_field(doc, "tx-power", "bridge", tx_power.bridge);
    let empty = doc
        .get("tx-power")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|table| table.is_empty());
    if empty {
        doc.remove("tx-power");
    }
}

/// Set one key of one table to `Some(value)`, or remove it for `None`. Creates
/// the table if a value is being set and it is not there yet.
///
/// A key already holding a value keeps its decor — the whitespace and any
/// trailing comment around it — rather than losing it to a freshly built item:
/// `table.insert` would otherwise replace the whole node, decor included, so
/// `fleet = 4   # quiet` becoming `fleet = 7` would lose the comment along with
/// the old number.
fn set_field(doc: &mut DocumentMut, table: &str, key: &str, value: Option<i8>) {
    match value {
        Some(value) => {
            let item = doc.entry(table).or_insert_with(toml_edit::table);
            let Some(table) = item.as_table_like_mut() else { return };
            let mut new_value = toml_edit::Value::from(i64::from(value));
            if let Some(decor) =
                table.get(key).and_then(toml_edit::Item::as_value).map(toml_edit::Value::decor)
            {
                *new_value.decor_mut() = decor.clone();
            }
            table.insert(key, toml_edit::Item::Value(new_value));
        }
        None => {
            if let Some(table) = doc.get_mut(table).and_then(toml_edit::Item::as_table_like_mut) {
                table.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, default_path_in, load, load_from, path, update};
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

    #[test]
    fn config_path_returns_explicit_when_given() {
        let explicit = std::path::PathBuf::from("/custom/wartui.toml");
        assert_eq!(path(Some(&explicit)), Some(explicit));
    }

    #[test]
    fn config_update_creates_file_and_parent_directories_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/deeper/wartui.toml");

        update(&target, |c| {
            c.tx_power.fleet = Some(10);
            c.tx_power.bridge = Some(15);
        })
        .unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(10));
        assert_eq!(saved.tx_power.bridge, Some(15));
    }

    #[test]
    fn config_update_keeps_existing_comments_when_writing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "# operator notes\n[tx-power]\nfleet = 6\n").unwrap();

        update(&target, |c| c.tx_power.fleet = Some(10)).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("# operator notes"), "{text}");
    }

    #[test]
    fn config_update_writes_both_keys_when_given() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        update(&target, |c| {
            c.tx_power.fleet = Some(8);
            c.tx_power.bridge = Some(12);
        })
        .unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(8));
        assert_eq!(saved.tx_power.bridge, Some(12));
    }

    #[test]
    fn config_update_refuses_out_of_range_value_and_leaves_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        let original = "[tx-power]\nfleet = 6\n";
        std::fs::write(&target, original).unwrap();

        let result = update(&target, |c| c.tx_power.fleet = Some(99));

        assert!(result.is_err());
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, original, "a refused write must not touch the file");
    }

    #[test]
    fn config_update_leaves_unrelated_hand_edit_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(
            &target,
            "[tx-power]\nfleet = 6\nbridge = 6\n\n# reminder: bench with headphones off\n",
        )
        .unwrap();

        update(&target, |c| c.tx_power.fleet = Some(10)).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("# reminder: bench with headphones off"), "{text}");
    }

    #[test]
    fn config_update_keeps_other_fields_when_changing_tx_power() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[tx-power]\nbridge = 15\n").unwrap();

        update(&target, |c| c.tx_power.fleet = Some(10)).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(10));
        assert_eq!(saved.tx_power.bridge, Some(15), "untouched by this update");
    }

    #[test]
    fn config_update_edits_inline_table_when_file_uses_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "tx-power = { fleet = 10 }\n").unwrap();

        update(&target, |c| c.tx_power.bridge = Some(15)).unwrap();

        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(10));
        assert_eq!(saved.tx_power.bridge, Some(15));
    }

    #[test]
    fn config_update_keeps_trailing_comment_when_changing_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[tx-power]\nfleet = 4   # quiet\n").unwrap();

        update(&target, |c| c.tx_power.fleet = Some(7)).unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("fleet = 7"), "{text}");
        assert!(text.contains("# quiet"), "{text}");
        assert!(
            text.lines().any(|line| line.contains("fleet = 7") && line.contains("# quiet")),
            "the comment stays on the same line: {text}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn config_update_writes_through_symlink_when_path_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::fs::write(&real, "[tx-power]\nfleet = 6\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        update(&link, |c| c.tx_power.fleet = Some(10)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        let saved = load(Some(&real)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(10), "the file it pointed to got the change");
    }

    #[test]
    #[cfg(unix)]
    fn config_update_creates_target_when_path_is_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap(); // target does not exist yet

        update(&link, |c| c.tx_power.fleet = Some(10)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        assert_eq!(std::fs::read_link(&link).unwrap(), target, "still points at the same place");
        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(10), "the link's target got created");
    }

    #[test]
    #[cfg(unix)]
    fn config_update_follows_a_relative_chain_of_dangling_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let middle = dir.path().join("middle.toml");
        let link = dir.path().join("wartui.toml");
        // `middle` names `real.toml` relative to its own directory, and neither
        // it nor `real.toml` exists yet.
        std::os::unix::fs::symlink("real.toml", &middle).unwrap();
        std::os::unix::fs::symlink(&middle, &link).unwrap();

        update(&link, |c| c.tx_power.fleet = Some(7)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(std::fs::symlink_metadata(&middle).unwrap().file_type().is_symlink());
        let saved = load(Some(&target)).unwrap();
        assert_eq!(saved.tx_power.fleet, Some(7), "the chain's final target got created");
    }

    #[test]
    fn config_update_leaves_file_untouched_when_change_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[tx-power]\nfleet = 10\n").unwrap();
        let before = std::fs::metadata(&target).unwrap().modified().unwrap();

        update(&target, |c| c.tx_power.fleet = Some(10)).unwrap();

        let after = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(before, after, "an identical document is never rewritten");
    }

    #[test]
    fn config_update_creates_no_file_when_change_sets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        update(&target, |_| {}).unwrap();

        assert!(!target.exists(), "nothing qualified, so nothing was written");
    }

    #[test]
    #[cfg(unix)]
    fn config_update_keeps_permissions_when_file_exists() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[tx-power]\nfleet = 6\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        update(&target, |c| c.tx_power.fleet = Some(10)).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the mode survives the rewrite");
    }
}
