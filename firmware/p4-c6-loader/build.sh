#!/usr/bin/env bash
# Builds the P4 loader with the C6 loader inside it.
#
#   build.sh [c6|p4]     no argument builds both, c6 first
#
# `c6` builds firmware/c6-loader, with `--features "$C6_LOADER_FEATURES"` when that is set, and
# saves its image as main/c6-loader.bin. `p4` builds this app around that image and merges it.
# The last line printed is the merged image's path, ready to write at 0x0. `p4` needs ESP-IDF
# v5.5.4's export.sh sourced first.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"

c6_elf="../c6-loader/target/riscv32imac-unknown-none-elf/release/wartui-c6-loader-fw"
c6_bin="main/c6-loader.bin"
# The factory C6's ota_1, where ESP-Hosted's OTA lands the image.
c6_max=1572864
merged="wartui-p4-c6-loader-esp32p4.bin"

build_c6() {
    (
        # The C6 loader pins its own toolchain and target directory.
        unset RUSTUP_TOOLCHAIN CARGO_TARGET_DIR
        cd ../c6-loader
        if [[ -n "${C6_LOADER_FEATURES:-}" ]]; then
            cargo build --release --locked --features "$C6_LOADER_FEATURES"
        else
            cargo build --release --locked
        fi
    )
    espflash save-image --skip-update-check --chip esp32c6 "$c6_elf" "$c6_bin"
    local size
    size=$(wc -c <"$c6_bin" | tr -d ' ')
    if ((size > c6_max)); then
        rm -f "$c6_bin"
        echo "c6-loader.bin is $size bytes, and the factory C6's ota_1 holds $c6_max" >&2
        exit 1
    fi
    echo "$c6_bin: $size bytes"
}

build_p4() {
    idf.py build
    idf.py merge-bin -o "$merged"
    echo "$here/build/$merged"
}

case "${1:-}" in
    "") build_c6; build_p4 ;;
    c6) build_c6 ;;
    p4) build_p4 ;;
    *) echo "usage: $0 [c6|p4]" >&2; exit 2 ;;
esac
