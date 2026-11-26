//! `wartui.toml`, the desktop's [`SettingsStore`]: where the file lives, and how a save replaces
//! it.
//!
//! An operator who names a file with `--config` gets an error when it is not there, not a silent
//! default. Without one, a missing default file is nothing stored yet, since most operators have
//! never written one, and with no `$HOME` there is no file at all.
//!
//! A save is atomic and keeps the file's shape on disk: a symlink stays one, a file keeps its
//! permissions, and a file holding an API key is readable only by its owner
//! ([`ConfigFile::save`]). The schema and its rules are `wartui_app::settings`'s.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use wartui_app::settings::SettingsStore;

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

/// The `wartui.toml` a command reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    path: PathBuf,
    /// Whether `--config` named it, so it must exist to load.
    required: bool,
}

impl ConfigFile {
    /// `explicit` (`--config`), which must exist to load, else [`default_path`], where a missing
    /// file loads as nothing stored. `None` with neither.
    #[must_use]
    pub fn new(explicit: Option<&Path>) -> Option<Self> {
        Self::resolve(explicit, default_path)
    }

    /// [`Self::new`], with where the default file lives supplied by the caller: a real lookup in
    /// `new`, a fixed path in a test, so the "no `--config`, file missing" case is exercised
    /// without mutating the process environment.
    #[must_use]
    pub fn resolve(
        explicit: Option<&Path>,
        default: impl FnOnce() -> Option<PathBuf>,
    ) -> Option<Self> {
        match explicit {
            Some(path) => Some(Self { path: path.to_path_buf(), required: true }),
            None => default().map(Self::at),
        }
    }

    /// The file at `path`, read as the default file is: a missing file loads as nothing stored.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path, required: false }
    }

    /// Where the file is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SettingsStore for ConfigFile {
    fn load(&self) -> io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if !self.required && error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Write `text` to the file, replacing whatever it held.
    ///
    /// Nothing on disk is read back or merged: a hand edit, a comment, or a file that does not
    /// parse is simply overwritten.
    ///
    /// The write lands in a temp file and is renamed into place — a torn write here
    /// would stop the next run from starting — beside the file the path *resolves to*
    /// rather than the path itself, so a path that is a symlink stays one: the rename
    /// replaces what it points at, not the link, even a dangling one. A file that
    /// existed keeps its permissions, copied onto the temp file before the rename; a
    /// new file is readable only by its owner on unix, since it can hold API keys.
    /// Saving any key (`secret`) into an existing file also drops its group and other bits, so a
    /// file written world-readable before it held one stops being so. The temp file is
    /// created fresh at `0600` on unix and loosened only after it is written, so no key is
    /// ever readable by others in passing.
    ///
    /// Skipped when the file already holds exactly this text: no temp file and no
    /// rename, though a file holding a key still has its permissions tightened in place.
    fn save(&self, text: &str, secret: bool) -> io::Result<()> {
        let path = self.path.as_path();
        if std::fs::read_to_string(path).is_ok_and(|held| held == text) {
            // Same text, but a key in a file others can read still has to be shut away.
            if secret
                && let Ok(meta) = std::fs::metadata(path)
                && let Some(tight) = owner_only_if(meta.permissions(), secret)
            {
                std::fs::set_permissions(path, tight)
                    .map_err(|error| step(error, "setting permissions on", path))?;
            }
            return Ok(());
        }

        // A path that does not exist yet has nothing to resolve or to have
        // permissions of, so it is used as given, and a new file gets the default.
        let target = resolve_symlink_target(path)?;
        let permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());

        if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| step(error, "creating", parent))?;
        }
        let mut temp_name = target.as_os_str().to_owned();
        temp_name.push(".tmp");
        let temp_path = PathBuf::from(temp_name);
        match std::fs::remove_file(&temp_path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(step(error, "removing", &temp_path));
            }
            _ => {}
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&temp_path)
            .and_then(|mut file| io::Write::write_all(&mut file, text.as_bytes()))
            .map_err(|error| step(error, "writing", &temp_path))?;
        let permissions = match permissions {
            Some(existing) => Some(owner_only_if(existing.clone(), secret).unwrap_or(existing)),
            None => owner_only(),
        };
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&temp_path, permissions)
                .map_err(|error| step(error, "setting permissions on", &temp_path))?;
        }
        std::fs::rename(&temp_path, &target).map_err(|error| step(error, "saving", &target))?;
        Ok(())
    }

    fn place(&self) -> String {
        self.path.display().to_string()
    }
}

/// `error`, saying which step of a save failed on `path`.
fn step(error: io::Error, doing: &str, path: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{doing} {}: {error}", path.display()))
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
/// link, and the fallback of using `path` itself would make a save replace
/// the link with a plain file instead of writing through it. A relative link
/// target is resolved against the link's own parent directory, the same way
/// a shell would. Bounded at 40 hops so a cycle errors rather than spinning.
fn resolve_symlink_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let Ok(metadata) = std::fs::symlink_metadata(&current) else { return Ok(current) };
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        let link =
            std::fs::read_link(&current).map_err(|error| step(error, "reading link", &current))?;
        current = if link.is_absolute() {
            link
        } else {
            current.parent().unwrap_or_else(|| Path::new("")).join(link)
        };
    }
    Err(io::Error::other(format!("too many levels of symbolic links: {}", path.display())))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use wartui_app::settings::{
        ApiKeys, Bridge, Config, Gps, Link, SettingsError, SettingsStore, load, serialize,
    };

    use super::{ConfigFile, default_path_in};

    /// The file at `path`, named as `--config` names one.
    fn named(path: &Path) -> ConfigFile {
        ConfigFile::resolve(Some(path), || None).expect("a named file")
    }

    /// Save `config` to `path`, as a front end saves it.
    fn save(path: &Path, config: &Config) -> std::io::Result<()> {
        let text = serialize(config).expect("valid settings");
        named(path).save(&text, config.api_keys.any_set())
    }

    /// The settings the file at `path` holds.
    fn reload(path: &Path) -> Result<Config, SettingsError> {
        load(&named(path))
    }

    #[test]
    fn config_file_path_uses_absolute_xdg_config_home_when_set() {
        let path = default_path_in(false, Some(OsStr::new("/home/op")), Some(OsStr::new("/x")));
        assert_eq!(path, Some("/x/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_file_path_ignores_relative_xdg_config_home_when_set() {
        let path =
            default_path_in(false, Some(OsStr::new("/home/op")), Some(OsStr::new("relative")));
        assert_eq!(path, Some("/home/op/.config/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_file_path_falls_back_to_dot_config_when_xdg_unset() {
        let path = default_path_in(false, Some(OsStr::new("/home/op")), None);
        assert_eq!(path, Some("/home/op/.config/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_file_path_uses_application_support_when_macos() {
        let path = default_path_in(true, Some(OsStr::new("/Users/op")), Some(OsStr::new("/x")));
        assert_eq!(path, Some("/Users/op/Library/Application Support/wartui/wartui.toml".into()));
    }

    #[test]
    fn config_file_path_returns_none_when_home_unset() {
        assert_eq!(default_path_in(false, None, None), None);
        assert_eq!(default_path_in(true, None, None), None);
    }

    #[test]
    fn config_file_loads_default_when_file_missing_and_no_path_given() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("wartui.toml");
        let file = ConfigFile::resolve(None, || Some(missing)).expect("the default file");
        assert!(file.load().unwrap().is_none(), "nothing stored yet");
        assert!(load(&file).unwrap().gps.bridge_rate_hz.is_none());
    }

    #[test]
    fn config_file_errors_when_explicit_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let error = reload(&path).unwrap_err();
        assert!(error.to_string().contains("missing.toml"), "{error}");
    }

    #[test]
    fn config_file_names_its_path_as_its_place() {
        let path = PathBuf::from("/custom/wartui.toml");
        assert_eq!(named(&path).place(), "/custom/wartui.toml");
        assert_eq!(named(&path).path(), path);
    }

    /// A `Config` holding every key the settings modal saves.
    fn full() -> Config {
        Config {
            excluded_ssids: vec!["HomeNet".to_owned(), "Pixel 9".to_owned()],
            bridge: Bridge {
                remember: Some(true),
                mac: Some([0xAA, 0xBB, 0xCC, 1, 2, 3]),
                serial: Some("AAAAAAAAAA".to_owned()),
                bluetooth_address: Some("02:00:5E:10:44:C1".to_owned()),
                link: Some(Link::Bluetooth),
            },
            gps: Gps { bridge_rate_hz: Some(2), wait_for_fix: Some(false) },
            api_keys: ApiKeys {
                wdgwars: "abc123def456".to_owned(),
                wigle_name: "AIDexamplename".to_owned(),
                wigle_token: "exampletoken789".to_owned(),
            },
        }
    }

    /// A `Config` with only the bridge's GPS rate set.
    fn rate_only(rate: u8) -> Config {
        Config { gps: Gps { bridge_rate_hz: Some(rate), wait_for_fix: None }, ..Config::default() }
    }

    #[test]
    fn config_file_save_creates_file_and_parent_directories_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/deeper/wartui.toml");

        save(&target, &full()).unwrap();

        let saved = reload(&target).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(2));
    }

    #[test]
    fn config_file_save_replaces_everything_when_file_holds_other_values() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(
            &target,
            "# notes\nexcluded-ssids = [\"HomeNet\"]\n[bridge]\nremember = false\n",
        )
        .unwrap();

        save(&target, &rate_only(3)).unwrap();

        let saved = reload(&target).unwrap();
        assert!(saved.excluded_ssids.is_empty(), "the file holds only what was saved");
        assert_eq!(saved.bridge.remember, None);
        assert_eq!(saved.gps.bridge_rate_hz, Some(3));
    }

    #[test]
    fn config_file_save_overwrites_file_when_existing_file_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "this is [not toml\n").unwrap();

        save(&target, &full()).unwrap();

        let saved = reload(&target).unwrap();
        assert_eq!(saved.excluded_ssids, ["HomeNet", "Pixel 9"]);
    }

    #[test]
    #[cfg(unix)]
    fn config_file_save_writes_through_symlink_when_path_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::fs::write(&real, "[gps]\nbridge-rate-hz = 1\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        save(&link, &rate_only(3)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        let saved = reload(&real).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(3), "the file it pointed to got the change");
    }

    #[test]
    #[cfg(unix)]
    fn config_file_save_creates_target_when_path_is_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        let link = dir.path().join("wartui.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap(); // target does not exist yet

        save(&link, &rate_only(3)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        assert_eq!(std::fs::read_link(&link).unwrap(), target, "still points at the same place");
        let saved = reload(&target).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(3), "the link's target got created");
    }

    #[test]
    #[cfg(unix)]
    fn config_file_save_follows_a_relative_chain_of_dangling_links() {
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
        let saved = reload(&target).unwrap();
        assert_eq!(saved.gps.bridge_rate_hz, Some(4), "the chain's final target got created");
    }

    #[test]
    fn config_file_save_leaves_file_untouched_when_it_already_holds_the_same_text() {
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
    fn config_file_save_keeps_permissions_when_file_exists() {
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
    #[cfg(unix)]
    fn config_file_save_creates_owner_only_file_when_file_is_new() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");

        save(&target, &Config::default()).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the file can hold an API key");
    }

    #[test]
    #[cfg(unix)]
    fn config_file_save_drops_group_and_other_bits_when_key_saved_into_open_file() {
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
    fn config_file_save_drops_group_and_other_bits_when_only_wigle_token_saved_into_open_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wartui.toml");
        std::fs::write(&target, "[gps]\nbridge-rate-hz = 1\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let config = Config {
            api_keys: ApiKeys { wigle_token: "exampletoken789".to_owned(), ..ApiKeys::default() },
            ..Config::default()
        };

        save(&target, &config).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a token is readable only by its owner");
    }

    #[test]
    #[cfg(unix)]
    fn config_file_save_drops_group_and_other_bits_when_text_unchanged_and_key_set() {
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
    fn config_file_save_keeps_open_mode_when_key_is_empty() {
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
    fn config_file_save_replaces_stale_temp_file_when_one_is_left_over() {
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
        assert_eq!(reload(&target).unwrap().api_keys.wdgwars, "abc123def456");
    }

    #[test]
    fn config_file_save_names_the_step_when_directory_cannot_be_created() {
        // A regular file where the config's directory should be.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();

        let error = save(&blocker.join("wartui.toml"), &rate_only(3)).unwrap_err();

        assert!(error.to_string().starts_with("creating "), "{error}");
        assert!(error.to_string().contains("not-a-dir"), "{error}");
    }
}
