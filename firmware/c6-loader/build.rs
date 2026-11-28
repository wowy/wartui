// Linker setup for esp-hal, taken from `esp-generate` 1.3.0's template. The
// `linker_be_nice` hook turns the linker's undefined-symbol errors into the
// advice you actually need, which is worth keeping verbatim so it stays easy to
// diff against upstream.
//
// The script also embeds what a wired flash of `firmware/p4-radio` writes: its app image, and the
// bootloader and partition table from its merged image. `radio-image` builds them, so the P4
// bridge's build embeds the same app.

use std::env;
use std::fs;
use std::path::PathBuf;

/// The merged image's layout: the table sector, then the app.
const TABLE_AT: usize = 0x8000;
const TABLE_END: usize = 0x9000;
const APP_AT: usize = 0x10000;

fn main() {
    linker_be_nice();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    bundle();
}

/// Build p4-radio and write `bundle.rs` into `OUT_DIR`: the app image and its SHA-256, and the
/// merged image's bootloader region and table sector, each saved as a file and included.
fn bundle() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let radio = wartui_radio_image::build(&out, true);
    let merged = radio.merged.expect("radio-image saves the merged image when asked");
    assert!(
        merged.len() >= APP_AT,
        "the merged radio image is {} bytes, short of the app at {APP_AT:#x}",
        merged.len()
    );
    // The loader writes these at 0x0 and 0x8000, where a C6 boots from.
    assert!(
        merged[0] == 0xE9,
        "the merged radio image starts with {:#04x}, not the bootloader's magic 0xE9",
        merged[0]
    );
    assert!(
        merged[TABLE_AT..TABLE_AT + 2] == [0xAA, 0x50],
        "the merged radio image's table at {TABLE_AT:#x} starts with {:02x?}, not the entry \
         magic [aa, 50]",
        &merged[TABLE_AT..TABLE_AT + 2]
    );
    let bootloader = out.join("bootloader.bin");
    let table = out.join("table.bin");
    fs::write(&bootloader, &merged[..TABLE_AT]).expect("write bootloader.bin");
    fs::write(&table, &merged[TABLE_AT..TABLE_END]).expect("write table.bin");
    let (app, sha256) = (&radio.app_path, radio.sha256);
    let source = format!(
        "/// p4-radio's app image, as `espflash save-image` writes it.\n\
         pub static APP: &[u8] = include_bytes!({app:?});\n\
         /// SHA-256 of [`APP`].\n\
         pub const APP_SHA256: [u8; 32] = {sha256:?};\n\
         /// The merged image below the table: the bootloader, padded with 0xFF.\n\
         pub static BOOTLOADER: &[u8; 0x8000] = include_bytes!({bootloader:?});\n\
         /// The merged image's table sector: p4-radio's `partitions.csv`, padded with 0xFF.\n\
         pub static TABLE: &[u8; 0x1000] = include_bytes!({table:?});\n"
    );
    fs::write(out.join("bundle.rs"), source).expect("write bundle.rs");
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
