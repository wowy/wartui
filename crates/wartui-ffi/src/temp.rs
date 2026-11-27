//! Whether SQLite finds a directory for an export's temporary files.
//!
//! **An export sorts through temporary files** (`wartui_core::export` `//!`), and the phone
//! package builds SQLite to keep them on disk on Android too (`package/build.sh`). SQLite looks in
//! `SQLITE_TMPDIR`, then `TMPDIR`, then system directories an Android app's process cannot write.
//! The app sets `SQLITE_TMPDIR`: this library cannot, since `std::env::set_var` is `unsafe`, and
//! `PRAGMA temp_store_directory` is deprecated and not thread-safe beside a running capture.
//!
//! **Checked up front.** A small export never spills, so a missing directory would pass every test
//! and fail an export of a long drive part way through. An export and an upload's prepare refuse
//! at the start instead, on Android only: iOS gives every app a `TMPDIR`, and a desktop SQLite
//! falls back to `/var/tmp`.
//!
//! **Usable means a file was created and removed there**, which is what SQLite does there, rather
//! than that the permissions read right.

use std::ffi::OsString;
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The variables SQLite reads for its temporary directory, in its order.
const VARIABLES: [&str; 2] = ["SQLITE_TMPDIR", "TMPDIR"];

/// Whether the process environment names a directory SQLite can put temporary files in.
pub(crate) fn found() -> bool {
    found_in(|name| std::env::var_os(name))
}

/// Whether `SQLITE_TMPDIR`, else `TMPDIR`, as `lookup` reads them, names a usable directory.
fn found_in(lookup: impl Fn(&str) -> Option<OsString>) -> bool {
    // The first usable one, as SQLite takes it. An empty value is no directory; joined, it would
    // probe the working directory.
    VARIABLES.iter().filter_map(|name| lookup(name)).any(|dir| !dir.is_empty() && usable(&dir))
}

/// Whether a file can be created in `dir` and removed again.
fn usable(dir: impl AsRef<Path>) -> bool {
    // Unique within the process, so two checks at once never share a probe.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let probe = dir.as_ref().join(format!(".wartui-temp-{}-{n}", std::process::id()));
    match File::create_new(&probe) {
        Ok(file) => {
            drop(file);
            fs::remove_file(&probe).is_ok()
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;

    use super::found_in;

    /// An environment of `vars` alone.
    fn env(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: HashMap<String, OsString> =
            vars.iter().map(|(name, dir)| (name.to_string(), dir.as_os_str().into())).collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn temp_check_passes_when_sqlite_tmpdir_is_writable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(found_in(env(&[("SQLITE_TMPDIR", dir.path())])));
        // The probe is removed.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn temp_check_falls_through_to_tmpdir_when_sqlite_tmpdir_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(!found_in(env(&[("SQLITE_TMPDIR", &missing)])));
        assert!(found_in(env(&[("SQLITE_TMPDIR", &missing), ("TMPDIR", dir.path())])));
    }

    #[cfg(unix)]
    #[test]
    fn temp_check_falls_through_to_tmpdir_when_sqlite_tmpdir_is_unwritable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let read_only = tempfile::tempdir().unwrap();
        std::fs::set_permissions(read_only.path(), PermissionsExt::from_mode(0o555)).unwrap();
        // Root writes whatever the mode says, which leaves nothing to tell apart.
        if std::fs::File::create(read_only.path().join("root")).is_ok() {
            return;
        }
        let only = found_in(env(&[("SQLITE_TMPDIR", read_only.path())]));
        let both = found_in(env(&[("SQLITE_TMPDIR", read_only.path()), ("TMPDIR", dir.path())]));
        // Writable again, so the directory can be removed.
        std::fs::set_permissions(read_only.path(), PermissionsExt::from_mode(0o755)).unwrap();
        assert!(!only);
        assert!(both);
    }

    #[test]
    fn temp_check_falls_through_to_tmpdir_when_sqlite_tmpdir_is_empty() {
        // The working directory is writable here, so a probe of it would pass.
        assert!(!found_in(env(&[("SQLITE_TMPDIR", Path::new(""))])));
        let dir = tempfile::tempdir().unwrap();
        assert!(found_in(env(&[("SQLITE_TMPDIR", Path::new("")), ("TMPDIR", dir.path())])));
    }

    #[test]
    fn temp_check_fails_when_neither_is_set() {
        assert!(!found_in(env(&[])));
    }
}
