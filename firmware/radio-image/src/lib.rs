//! One build of `firmware/p4-radio` for every image that carries it.
//!
//! The P4 bridge and the C6 loader both embed the C6's app image. Each build script calls
//! [`build`] rather than keeping its own copy, so an input list or a flash argument changed
//! here changes both images together and their digests cannot drift apart.
//!
//! The C6 is built by its own cargo, from its own directory, so its toolchain pin, target and
//! `.cargo/config.toml` apply rather than the caller's: every variable the outer cargo sets
//! for a build script is cleared first.
//!
//! The C6's app descriptor embeds a build time unless `SOURCE_DATE_EPOCH` is set, so two builds
//! of one tree would differ, and a P4 bridge built beside the C6 loader would reflash the C6 it
//! set up. When the caller sets no `SOURCE_DATE_EPOCH`, the build gets the time of the last
//! commit that touched [`RADIO_INPUTS`]. Nothing reruns a build script when only `HEAD` moves, so
//! the time must move only with what reruns it: two builds then agree whenever their radio
//! inputs do, a cached image included, and the digest holds across releases that leave p4-radio
//! alone. A radio input with uncommitted changes gets the current time and a warning, since a
//! dirty tree builds no reproducible image either way.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use wartui_proto::ota::image::appended_digest_of;

/// Where the app starts in the merged image, as `partitions.csv` places the app slot.
const APP_OFFSET: usize = 0x10000;

/// p4-radio's images, saved with its runner's flash mode, frequency and size.
pub struct Radio {
    /// The app-only image, as one app slot holds it.
    pub app: Vec<u8>,
    /// Where `app` was saved, for `include_bytes!`.
    pub app_path: PathBuf,
    /// The merged image (bootloader at 0x0, table at 0x8000, app at 0x10000), when asked for.
    pub merged: Option<Vec<u8>>,
    /// SHA-256 of `app`, which the C6's receiver checks.
    pub sha256: [u8; 32],
    /// The digest espflash appended to `app`, which names the build.
    pub digest: [u8; 32],
}

/// What the C6's build reads, relative to this crate: what reruns the build, and what dates it.
/// Its `target` directories are left out: a change there is the build's output, not its input.
const RADIO_INPUTS: &[&str] = &[
    "../p4-radio/src",
    "../p4-radio/build.rs",
    "../p4-radio/Cargo.toml",
    "../p4-radio/Cargo.lock",
    "../p4-radio/partitions.csv",
    "../p4-radio/rust-toolchain.toml",
    "../p4-radio/.cargo",
    "../common/src",
    "../common/Cargo.toml",
    "../../crates/wartui-proto/src",
    "../../crates/wartui-proto/Cargo.toml",
];

/// The C6's build-time settings (`firmware/p4-radio/src/config.rs`), passed through.
const RADIO_ENV: &[&str] = &["WARTUI_RADIO_UP_QUEUE_BYTES", "WARTUI_RADIO_CONN_INTERVAL_MS"];

/// The build time when git cannot say: 2026-11-28T00:00:00Z.
const FALLBACK_EPOCH: &str = "1795824000";

/// Variables the outer cargo sets for a build script that would steer the C6's build, beside
/// every `CARGO_*` but `CARGO_HOME`.
const OUTER: &[&str] = &[
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_LINKER",
    "RUSTDOC",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTUP_TOOLCHAIN",
    "TARGET",
    "HOST",
    "OUT_DIR",
    "OPT_LEVEL",
    "PROFILE",
    "DEBUG",
    "NUM_JOBS",
];

/// Build `firmware/p4-radio` and save its images into `out_dir`: `radio.bin`, and with
/// `merged` also `radio-merged.bin`. Prints every `cargo:rerun-if-changed` and
/// `cargo:rerun-if-env-changed` line, so call it from a build script.
///
/// # Panics
///
/// When the build or the save fails, or the saved image carries no appended digest, with the
/// output and a hint at the usual cause.
pub fn build(out_dir: &Path, merged: bool) -> Radio {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for path in RADIO_INPUTS {
        println!("cargo:rerun-if-changed={}", here.join(path).display());
    }
    for var in RADIO_ENV.iter().chain(&["SOURCE_DATE_EPOCH"]) {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let radio = here.join("../p4-radio");
    let target_dir = radio.join("target");

    let mut build = Command::new("cargo");
    build.current_dir(&radio).args(["build", "--release", "--locked"]);
    build.arg("--target-dir").arg(&target_dir);
    for (key, _) in env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if (key.starts_with("CARGO_") && key != "CARGO_HOME") || OUTER.contains(&key) {
            build.env_remove(key);
        }
    }
    if env::var_os("SOURCE_DATE_EPOCH").is_none() {
        build.env("SOURCE_DATE_EPOCH", build_time(&here));
    }
    run(
        build,
        "building the radio image (firmware/p4-radio) failed",
        "`cargo` is not on the PATH",
        &[
            (
                "may not be installed",
                "the C6's target is missing: `rustup target add riscv32imac-unknown-none-elf`",
            ),
            (
                "--locked was passed",
                "firmware/p4-radio/Cargo.lock is out of date: run `cargo update --workspace` \
                 in firmware/p4-radio",
            ),
        ],
    );

    let elf = target_dir.join("riscv32imac-unknown-none-elf/release/wartui-p4-radio-fw");
    let save = |bin: &Path, extra: &[&str]| {
        let mut save = Command::new("espflash");
        save.args(["save-image", "--skip-update-check", "--chip", "esp32c6"])
            // As the wired flash's runner in `firmware/p4-radio/.cargo/config.toml` writes
            // them: the header is under the digest, so a difference reflashes a C6 already on
            // this build.
            .args(["--flash-mode", "dio", "--flash-freq", "40mhz", "--flash-size", "4mb"])
            .args(extra)
            .arg("--partition-table")
            .arg(radio.join("partitions.csv"))
            .arg(&elf)
            .arg(bin);
        run(
            save,
            "saving the radio image failed",
            "`espflash` is not installed; the build needs it: `cargo install espflash --locked`",
            &[],
        );
    };

    let app_path = out_dir.join("radio.bin");
    save(&app_path, &[]);
    let app = fs::read(&app_path).expect("read the saved radio image");
    let sha256: [u8; 32] = Sha256::digest(&app).into();
    let Some(digest) = appended_digest_of(&app) else {
        panic!(
            "the radio image {} carries no appended SHA-256 digest, which is how the P4 \
             tells whether its C6 runs it",
            app_path.display()
        );
    };

    let merged = merged.then(|| {
        let path = out_dir.join("radio-merged.bin");
        save(&path, &["--merge", "--skip-padding"]);
        let merged = fs::read(&path).expect("read the saved merged radio image");
        assert!(
            merged.get(APP_OFFSET..APP_OFFSET + app.len()) == Some(app.as_slice()),
            "the merged radio image's app differs from the app-only image"
        );
        merged
    });

    Radio { app, app_path, merged, sha256, digest }
}

/// The time to build the radio image at, in seconds, from git run in `dir`: the last commit
/// that touched [`RADIO_INPUTS`], the current time when one has uncommitted changes, and
/// [`FALLBACK_EPOCH`] when git cannot say.
fn build_time(dir: &Path) -> String {
    let git = |args: Vec<String>| {
        let output = Command::new("git").current_dir(dir).args(args).output().ok()?;
        output.status.success().then_some(output.stdout)
    };
    let dirty = git(git_args(&["status", "--porcelain"], RADIO_INPUTS))
        .is_some_and(|stdout| !stdout.is_empty());
    if dirty {
        println!(
            "cargo:warning=the radio image's inputs have uncommitted changes, so it is built at \
             the current time and matches no other build"
        );
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        return now.to_string();
    }
    let time = git(git_args(&["log", "-1", "--format=%ct"], RADIO_INPUTS)).and_then(|stdout| {
        let text = String::from_utf8(stdout).ok()?;
        let text = text.trim();
        text.parse::<u64>().is_ok().then(|| text.to_owned())
    });
    time.unwrap_or_else(|| FALLBACK_EPOCH.to_owned())
}

/// The git command `command`, limited to the `inputs` paths.
fn git_args(command: &[&str], inputs: &[&str]) -> Vec<String> {
    command.iter().chain(&["--"]).chain(inputs).map(|arg| (*arg).to_owned()).collect()
}

/// Run `command`, failing the build with `why` and its output if it fails, and with `missing`
/// if the program is not there. Each `(pattern, hint)` whose pattern the stderr holds adds its
/// hint after the output.
fn run(mut command: Command, why: &str, missing: &str, hints: &[(&str, &str)]) {
    let output = match command.output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => panic!("{missing}"),
        Err(err) => panic!("could not run {:?}: {err}", command.get_program()),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut message = format!(
            "{why}\n--- stdout\n{}\n--- stderr\n{stderr}",
            String::from_utf8_lossy(&output.stdout)
        );
        for (pattern, hint) in hints {
            if stderr.contains(pattern) {
                message.push_str("\nhint: ");
                message.push_str(hint);
            }
        }
        panic!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_time_limits_git_to_radio_inputs_when_dating_the_build() {
        let args = git_args(&["log", "-1", "--format=%ct"], &["../p4-radio/src", "../common/src"]);
        assert_eq!(args, ["log", "-1", "--format=%ct", "--", "../p4-radio/src", "../common/src"]);
    }
}
