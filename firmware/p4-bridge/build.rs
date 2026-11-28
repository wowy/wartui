// Linker setup for esp-hal, taken from `esp-generate` 1.3.0's template. The
// `linker_be_nice` hook turns the linker's undefined-symbol errors into the
// advice you actually need, which is worth keeping verbatim so it stays easy to
// diff against upstream.
//
// The script also embeds the C6's image (`firmware/p4-radio`), so the P4 carries the radio
// build it bridges and reflashes the C6 when that differs (`src/radio.rs`). `radio-image`
// builds it, so the C6 loader's build embeds the same bytes.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    linker_be_nice();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    bundle_radio();
}

/// Build the C6's app image and write `radio.rs` into `OUT_DIR`: the image, its whole-file
/// SHA-256 (what the C6's receiver checks) and the digest espflash appended (what names it).
fn bundle_radio() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let radio = wartui_radio_image::build(&out, false);
    let (bin, sha256, digest) = (&radio.app_path, radio.sha256, radio.digest);
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
