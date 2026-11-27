#!/usr/bin/env bash
# Package the binding for phones: the release AAR an Android app depends on, and the XCFramework
# and Swift an iOS app's Swift package builds on.
#
#   crates/wartui-ffi/package/build.sh [android|ios|all]    # default all
#
# Android: `libwartui_ffi.so` for arm64-v8a and x86_64 and the generated Kotlin, laid into
# `android/wartui-core/generated/`; the AAR, copied to `target/phone/android/wartui-core.aar`.
# iOS: `libwartui_ffi.a` for the device and the simulator, made into
# `swift/WartuiCoreFFI.xcframework`, and `swift/Sources/WartuiCore/WartuiCore.swift`. Everything
# else it makes stays under `target/phone/`. Git ignores all of it.
set -euo pipefail

which=${1:-all}
case "$which" in
  android | ios | all) ;;
  *)
    echo "usage: $0 [android|ios|all]" >&2
    exit 2
    ;;
esac

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ffi=$(cd "$here/.." && pwd)
root=$(cd "$ffi/../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
out=$target/phone

# The iOS release the static libraries and the Swift package both name as their minimum. Keep it
# equal to `platforms` in `swift/Package.swift`.
ios_minimum=17.0

fail() {
  echo "$*" >&2
  exit 1
}

# What a platform needs on PATH, named when missing.
need() {
  local missing=()
  for tool in "$@"; do
    command -v "$tool" > /dev/null || missing+=("$tool")
  done
  if ((${#missing[@]})); then
    fail "packaging for $which needs ${missing[*]} on PATH"
  fi
}

case "$which" in
  android) need cargo rustup java ;;
  ios) need cargo rustup xcodebuild ;;
  all) need cargo rustup java xcodebuild ;;
esac

# The bindings generator, built for the host from this crate's own `uniffi`.
(cd "$root" && cargo build -p wartui-ffi --features cli --locked)
bindgen=$target/debug/uniffi-bindgen

# Add a Rust target when rustup lacks it.
rust_target() {
  rustup target list --installed | grep -x "$1" > /dev/null || rustup target add "$1"
}

# Fail when the generated `binding` carries anything from the `testing` feature.
check_shipped_api() {
  local binding=$1
  if grep -q 'WireSim\|testingNodeImage\|testing_node_image' "$binding"; then
    fail "$binding exports the testing feature's API, which no shipped build enables"
  fi
}

## Android

android_project=$ffi/android
module=$android_project/wartui-core
catalog=$android_project/gradle/libs.versions.toml

# A version the Gradle build's catalog pins, so the two builds cannot disagree.
catalog_version() {
  local value
  value=$(sed -n "s/^$1 = \"\(.*\)\"$/\1/p" "$catalog")
  [[ -n $value ]] || fail "$catalog pins no $1"
  echo "$value"
}

# The NDK: `ANDROID_NDK_HOME`, else `ANDROID_NDK_LATEST_HOME`, else the newest under the SDK.
find_ndk() {
  local ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-}}
  if [[ -z $ndk ]]; then
    ndk=$(find "$android_sdk/ndk" -mindepth 1 -maxdepth 1 -type d 2> /dev/null |
      sort -V | tail -n 1)
  fi
  [[ -n $ndk && -d $ndk ]] ||
    fail "no NDK: set ANDROID_NDK_HOME, or install one under $android_sdk/ndk"
  echo "$ndk"
}

# The ABI directory Android loads a triple's library from.
abi_of() {
  case "$1" in
    aarch64-linux-android) echo arm64-v8a ;;
    x86_64-linux-android) echo x86_64 ;;
  esac
}

# Google Play requires every `LOAD` segment aligned to 16 KB pages.
check_pages() {
  local library=$1 align aligned=0
  while read -r align; do
    ((align >= 16384)) || fail "$library has a LOAD segment aligned to $align, not 16 KB"
    aligned=1
  done < <("$ndk_bin/llvm-readelf" -lW "$library" | awk '$1 == "LOAD" { print $NF }')
  ((aligned)) || fail "llvm-readelf found no LOAD segment in $library"
}

# Fail a library whose SQLite is not built with `SQLITE_TEMP_STORE=1`, temporary storage in files
# unless a connection asks otherwise: an export would hold its whole sort in memory
# (`crates/wartui-core/src/export.rs`). SQLite keeps its compile options as strings.
check_temp_store() {
  local library=$1
  # Not `grep -q`, which stops reading early and fails the pipe.
  "$ndk_bin/llvm-strings" "$library" | grep -x 'TEMP_STORE=1' > /dev/null ||
    fail "$library's SQLite is not built with SQLITE_TEMP_STORE=1, so an export would hold" \
      "its whole sort in memory"
}

# Build `libwartui_ffi.so` for `triple`, linked by the minimum API's clang.
build_android() {
  local triple=$1
  local upper lower clang
  upper=$(tr 'a-z-' 'A-Z_' <<< "$triple")
  lower=${triple//-/_}
  clang=$ndk_bin/$triple$min_sdk-clang
  [[ -x $clang ]] || fail "the NDK at $ndk has no $triple$min_sdk-clang"
  rust_target "$triple"
  # libsqlite3-sys builds SQLite for Android with `SQLITE_TEMP_STORE=3`, temporary storage always
  # in memory, and appends these flags after its own, so SQLite's default holds: files. The app
  # names their directory (`crates/wartui-ffi/src/temp.rs`).
  (cd "$root" && env \
    "CARGO_TARGET_${upper}_LINKER=$clang" \
    "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-arg=-Wl,-z,max-page-size=16384" \
    "CC_$lower=$clang" \
    "AR_$lower=$ndk_bin/llvm-ar" \
    "LIBSQLITE3_FLAGS=-USQLITE_TEMP_STORE -DSQLITE_TEMP_STORE=1" \
    cargo build -p wartui-ffi --release --target "$triple" --locked)
  # Stripped here, with the NDK it was linked by, so the AAR ships it small; the Gradle build
  # leaves it as it is.
  local abi shipped
  abi=$(abi_of "$triple")
  shipped=$module/generated/jniLibs/$abi/libwartui_ffi.so
  mkdir -p "$(dirname "$shipped")"
  "$ndk_bin/llvm-strip" --strip-unneeded -o "$shipped" \
    "$target/$triple/release/libwartui_ffi.so"
  check_pages "$shipped"
  check_temp_store "$shipped"
}

android() {
  android_sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
  [[ -d $android_sdk ]] || fail "no Android SDK at $android_sdk: set ANDROID_HOME"
  export ANDROID_HOME=$android_sdk
  ndk=$(find_ndk)
  echo "using the NDK at $ndk"
  case "$(uname -s)" in
    Darwin) ndk_bin=$ndk/toolchains/llvm/prebuilt/darwin-x86_64/bin ;;
    Linux) ndk_bin=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin ;;
    *) fail "the Android build runs on macOS and Linux" ;;
  esac
  min_sdk=$(catalog_version minSdk)

  rm -rf "$module/generated"
  build_android aarch64-linux-android
  build_android x86_64-linux-android

  # `android = true` gives the binding Android's object cleaner, from API 34, and imports
  # `android.os.Build`, so it is set in a config derived here rather than in `uniffi.toml`, which
  # the desktop smoke test reads too.
  mkdir -p "$out/android"
  local config=$out/android/uniffi.toml
  cat > "$config" << EOF
[crates.wartui_ffi.bindings.kotlin]
android = true
kotlin_target_version = "$(catalog_version kotlin)"
EOF
  (cd "$root" && "$bindgen" generate \
    --library "$target/aarch64-linux-android/release/libwartui_ffi.so" \
    --language kotlin --out-dir "$module/generated/kotlin" --config "$config" --no-format)
  check_shipped_api "$module/generated/kotlin/racing/deadbear/wartui/core/wartui_ffi.kt"

  (cd "$android_project" && ./gradlew :wartui-core:assembleRelease)
  cp "$module/build/outputs/aar/wartui-core-release.aar" "$out/android/wartui-core.aar"
  echo "OK android: $out/android/wartui-core.aar"
}

## iOS

swift_package=$ffi/swift

ios() {
  [[ $(uname -s) == Darwin ]] || fail "the iOS build runs on macOS"
  local triple
  for triple in aarch64-apple-ios aarch64-apple-ios-sim; do
    rust_target "$triple"
    (cd "$root" && IPHONEOS_DEPLOYMENT_TARGET=$ios_minimum \
      cargo build -p wartui-ffi --release --target "$triple" --locked)
  done
  local device=$target/aarch64-apple-ios/release/libwartui_ffi.a
  local simulator=$target/aarch64-apple-ios-sim/release/libwartui_ffi.a

  local bindings=$out/ios/bindings
  rm -rf "$bindings"
  (cd "$root" && "$bindgen" generate --library "$device" --language swift \
    --out-dir "$bindings" --no-format)
  check_shipped_api "$bindings/WartuiCore.swift"

  # Xcode reads a library's module map as `module.modulemap`. It sits in a directory of its own
  # inside `Headers`, because Xcode copies every XCFramework's headers into one directory, where
  # two `module.modulemap` files would collide.
  local headers=$out/ios/headers
  rm -rf "$headers"
  mkdir -p "$headers/WartuiCoreFFI"
  cp "$bindings/WartuiCoreFFI.h" "$headers/WartuiCoreFFI/"
  cp "$bindings/WartuiCoreFFI.modulemap" "$headers/WartuiCoreFFI/module.modulemap"

  local xcframework=$swift_package/WartuiCoreFFI.xcframework
  rm -rf "$xcframework"
  xcodebuild -create-xcframework \
    -library "$device" -headers "$headers" \
    -library "$simulator" -headers "$headers" \
    -output "$xcframework"
  mkdir -p "$swift_package/Sources/WartuiCore"
  cp "$bindings/WartuiCore.swift" "$swift_package/Sources/WartuiCore/"
  echo "OK ios: $xcframework"
}

case "$which" in
  android) android ;;
  ios) ios ;;
  all)
    android
    ios
    ;;
esac
