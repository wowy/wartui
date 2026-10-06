//! A source identity includes dirty files: a Git revision cannot name what the host compiled.
//! Paths and lengths delimit the sorted contents so checkout location and traversal order do not
//! matter. FNV-1a is a noncryptographic label, not proof of a binary or a node's firmware.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct HostSources {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
}

pub fn host_sources(workspace: &Path) -> io::Result<HostSources> {
    let mut sources = HostSources {
        files: vec![PathBuf::from("Cargo.toml"), PathBuf::from("Cargo.lock")],
        directories: Vec::new(),
    };
    collect(workspace, Path::new("crates"), &mut sources)?;
    sources.files.sort();
    sources.directories.sort();
    Ok(sources)
}

fn collect(workspace: &Path, relative: &Path, sources: &mut HostSources) -> io::Result<()> {
    sources.directories.push(relative.to_owned());
    for entry in fs::read_dir(workspace.join(relative))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        let path = relative.join(&name);
        if kind.is_dir() {
            if !matches!(name.to_str(), Some("target" | ".git" | ".junie" | "firmware")) {
                collect(workspace, &path, sources)?;
            }
        } else if kind.is_file()
            && (name == "Cargo.toml" || path.extension().is_some_and(|ext| ext == "rs"))
        {
            sources.files.push(path);
        }
    }
    Ok(())
}

fn feed(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

pub fn build_id(workspace: &Path, sources: &HostSources) -> io::Result<String> {
    let mut hash = 0xcbf29ce484222325;
    for relative in &sources.files {
        let path = relative
            .components()
            .map(|part| {
                part.as_os_str().to_str().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 source path")
                })
            })
            .collect::<io::Result<Vec<_>>>()?
            .join("/");
        let contents = fs::read(workspace.join(relative))?;
        feed(&mut hash, &(path.len() as u64).to_le_bytes());
        feed(&mut hash, path.as_bytes());
        feed(&mut hash, &(contents.len() as u64).to_le_bytes());
        feed(&mut hash, &contents);
    }
    Ok(format!("fnv1a64:{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let workspace = option_env!("CARGO_MANIFEST_DIR")
                .map(|manifest| Path::new(manifest).parent().unwrap().parent().unwrap().to_owned())
                .unwrap_or_else(|| std::env::current_dir().unwrap());
            let root = workspace.join("target/build-provenance-tests").join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let fixture = Self(root);
            fixture.write("Cargo.toml", "workspace");
            fixture.write("Cargo.lock", "lock");
            fixture.write("crates/host/Cargo.toml", "host");
            fixture.write("crates/host/src/main.rs", "fn main() {}");
            fixture.write("crates/host/build.rs", "fn main() {}");
            fixture
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn id(&self) -> String {
            build_id(&self.0, &host_sources(&self.0).unwrap()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn build_provenance_matches_fnv1a_when_input_is_known() {
        let mut hash = 0xcbf29ce484222325;
        feed(&mut hash, b"hello");
        assert_eq!(hash, 0xa430d84680aabd0b);
    }

    #[test]
    fn build_provenance_is_deterministic_when_checkout_and_creation_order_differ() {
        let first = Fixture::new();
        let second = Fixture::new();
        first.write("crates/host/src/z.rs", "z");
        first.write("crates/host/src/a.rs", "a");
        second.write("crates/host/src/a.rs", "a");
        second.write("crates/host/src/z.rs", "z");
        assert_eq!(first.id(), second.id());
        let sources = host_sources(&first.0).unwrap();
        assert!(sources.files.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(sources.directories.contains(&PathBuf::from("crates/host/src")));
    }

    #[test]
    fn build_provenance_changes_when_host_inputs_change() {
        let fixture = Fixture::new();
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "crates/host/Cargo.toml",
            "crates/host/build.rs",
            "crates/host/src/main.rs",
        ] {
            let before = fixture.id();
            fixture.write(path, "changed");
            assert_ne!(before, fixture.id(), "{path}");
        }
        let before = fixture.id();
        fixture.write("crates/host/src/new.rs", "new");
        assert_ne!(before, fixture.id());
        fs::remove_file(fixture.0.join("crates/host/src/new.rs")).unwrap();
        assert_eq!(before, fixture.id());
        fs::rename(
            fixture.0.join("crates/host/src/main.rs"),
            fixture.0.join("crates/host/src/renamed.rs"),
        )
        .unwrap();
        assert_ne!(before, fixture.id());
    }

    #[test]
    fn build_provenance_ignores_non_host_inputs_when_their_contents_change() {
        let fixture = Fixture::new();
        let before = fixture.id();
        for path in [
            "firmware/node/src/main.rs",
            "target/generated.rs",
            ".git/config",
            ".junie/notes.rs",
            "capture.db",
            "crates/host/capture.db",
            "crates/host/target/generated.rs",
            "crates/host/.git/ignored.rs",
            "crates/host/.junie/ignored.rs",
            "crates/host/firmware/ignored.rs",
        ] {
            fixture.write(path, "ignored");
        }
        assert_eq!(before, fixture.id());
    }
}
