// Linker setup for esp-hal, taken from `esp-generate` 1.3.0's template. The
// `linker_be_nice` hook turns the linker's undefined-symbol errors into the
// advice you actually need, which is worth keeping verbatim so it stays easy to
// diff against upstream.

fn main() {
    // `linker_be_nice` registers itself with `--error-handling-script`, which is
    // an LLD option. The RISC-V parts link with `rust-lld` and get the nicer
    // messages; the S3 links with `xtensa-esp32s3-elf-gcc`, which rejects the
    // flag outright — so a hook whose whole job is to explain link errors would
    // instead *be* the link error, on every build, saying nothing about the
    // program. Skipped there rather than rewritten, so the function below stays
    // diffable against `esp-generate`.
    //
    // The variable is also absent when the linker invokes this binary as the
    // script rather than cargo invoking it as a build script; that reads as
    // "not xtensa" and calls through, which is what that path needs.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("xtensa") {
        linker_be_nice();
    }
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");
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
