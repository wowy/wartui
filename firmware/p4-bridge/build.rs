// Linker setup for esp-hal, taken from `esp-generate` 1.3.0's template. The
// `linker_be_nice` hook turns the linker's undefined-symbol errors into the
// advice you actually need, which is worth keeping verbatim so it stays easy to
// diff against upstream.
//
// The script also builds the C6's image (`firmware/p4-radio`) and embeds it, so the
// P4 carries the radio build it bridges and reflashes the C6 when that differs
// (`src/radio.rs`). The C6 is built by its own cargo, from its own directory, so its
// toolchain pin, target and `.cargo/config.toml` apply rather than this build's: every
// variable the outer cargo sets for a build script is cleared first.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};
use wartui_proto::ota::image::appended_digest;

fn main() {
    linker_be_nice();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    bundle_radio();
}

/// What the C6's build reads, relative to this crate. Its `target` directories are left
/// out: a change there is the build's output, not its input.
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

/// The C6's build-time queue sizes (`firmware/p4-radio/src/config.rs`), passed through.
const RADIO_ENV: &[&str] = &[
    "WARTUI_RADIO_UP_QUEUE_BYTES",
    "WARTUI_RADIO_UP_PRIORITY_BYTES",
    "WARTUI_RADIO_SEND_JOBS",
    "WARTUI_RADIO_JOBS",
];

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

/// Build the C6's app image and write `radio.rs` into `OUT_DIR`: the image, its whole-file
/// SHA-256 (what the C6's receiver checks) and the digest espflash appended (what names it).
fn bundle_radio() {
    for path in RADIO_INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    for var in RADIO_ENV {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let here = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let radio = here.join("../p4-radio");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target_dir = radio.join("target");

    let mut build = Command::new("cargo");
    build.current_dir(&radio).args(["build", "--release", "--features", "esp32c6", "--locked"]);
    build.arg("--target-dir").arg(&target_dir);
    for (key, _) in env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if (key.starts_with("CARGO_") && key != "CARGO_HOME") || OUTER.contains(&key) {
            build.env_remove(key);
        }
    }
    run(
        build,
        "building the radio image (firmware/p4-radio) failed; it needs the C6's target: \
         `rustup target add riscv32imac-unknown-none-elf`",
        "`cargo` is not on the PATH",
    );

    let elf = target_dir.join("riscv32imac-unknown-none-elf/release/wartui-p4-radio-fw");
    let bin = out.join("radio.bin");
    let mut save = Command::new("espflash");
    save.args(["save-image", "--skip-update-check", "--chip", "esp32c6", "--partition-table"])
        .arg(radio.join("partitions.csv"))
        .arg(&elf)
        .arg(&bin);
    run(
        save,
        "saving the radio image failed",
        "`espflash` is not installed; the P4 build needs it: `cargo install espflash --locked`",
    );

    let image = fs::read(&bin).expect("read the saved radio image");
    let sha256: [u8; 32] = Sha256::digest(&image).into();
    let Some(digest) = appended_digest(|at, buf| read_at(&image, at, buf)) else {
        panic!(
            "the radio image {} carries no appended SHA-256 digest, which is how the P4 \
             tells whether its C6 runs it",
            bin.display()
        );
    };
    let source = format!(
        "/// The C6's app image, as `espflash save-image` writes it.\n\
         pub static RADIO_IMAGE: &[u8] = include_bytes!({bin:?});\n\
         /// SHA-256 of [`RADIO_IMAGE`], which the C6's receiver checks.\n\
         pub const RADIO_SHA256: [u8; 32] = {sha256:?};\n\
         /// The digest espflash appended to [`RADIO_IMAGE`], which names the build.\n\
         pub const RADIO_DIGEST: [u8; 32] = {digest:?};\n"
    );
    fs::write(out.join("radio.rs"), source).expect("write radio.rs");
}

/// Run `command`, failing the build with `why` and its output if it fails, and with `missing`
/// if the program is not there.
fn run(mut command: Command, why: &str, missing: &str) {
    let output = match command.output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => panic!("{missing}"),
        Err(err) => panic!("could not run {:?}: {err}", command.get_program()),
    };
    if !output.status.success() {
        panic!(
            "{why}\n--- stdout\n{}\n--- stderr\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Fill `buf` from `image` at `at`, or return `false` past its end.
fn read_at(image: &[u8], at: u32, buf: &mut [u8]) -> bool {
    let start = at as usize;
    match image.get(start..start + buf.len()) {
        Some(bytes) => {
            buf.copy_from_slice(bytes);
            true
        }
        None => false,
    }
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "💡 `embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests"
                    );
                    eprintln!();
                }
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
