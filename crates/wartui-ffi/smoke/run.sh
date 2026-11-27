#!/usr/bin/env bash
# The binding's smoke tests: build the library, generate its Kotlin and Swift bindings, and run a
# program in each language that captures through it.
#
#   crates/wartui-ffi/smoke/run.sh [kotlin|swift|all]    # default all
#
# Prints `OK <language>` for each that passes, and exits non-zero on any failure. Needs `kotlinc`
# and `java` for Kotlin, and `swiftc` 6.4 or later for Swift. Everything it makes stays under
# `target/ffi-smoke/`.
set -euo pipefail

which=${1:-all}
case "$which" in
  kotlin | swift | all) ;;
  *)
    echo "usage: $0 [kotlin|swift|all]" >&2
    exit 2
    ;;
esac

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/../../.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
out=$target/ffi-smoke

case "$(uname -s)" in
  Darwin) library=$target/debug/libwartui_ffi.dylib ;;
  Linux) library=$target/debug/libwartui_ffi.so ;;
  *)
    echo "the smoke tests run on macOS and Linux" >&2
    exit 1
    ;;
esac

# What each language needs on PATH, named when missing.
need() {
  local missing=()
  for tool in "$@"; do
    command -v "$tool" > /dev/null || missing+=("$tool")
  done
  if ((${#missing[@]})); then
    echo "the $which smoke test needs ${missing[*]} on PATH" >&2
    exit 1
  fi
}

case "$which" in
  kotlin) need cargo kotlinc java ;;
  swift) need cargo swiftc ;;
  all) need cargo kotlinc java swiftc ;;
esac

(cd "$root" && cargo build -p wartui-ffi --features testing --locked)
(cd "$root" && cargo build -p wartui-bindgen --locked)
bindgen=$target/debug/uniffi-bindgen

# Generate `language`'s binding from the built library into `target/ffi-smoke/<language>/`.
generate() {
  local language=$1
  rm -rf "$out/$language"
  (cd "$root" && "$bindgen" generate --library "$library" --language "$language" \
    --out-dir "$out/$language" --no-format)
}

# JNA and kotlinx-coroutines from Maven Central, each checked against its pinned digest.
jar() {
  local path=$1 name=$2 sum=$3
  local file=$out/jars/$name
  mkdir -p "$out/jars"
  if [[ ! -f $file ]]; then
    curl --fail --silent --show-error --location --output "$file.part" \
      "https://repo1.maven.org/maven2/$path/$name"
    mv "$file.part" "$file"
  fi
  local got
  if command -v sha256sum > /dev/null; then
    got=$(sha256sum "$file" | cut -d' ' -f1)
  else
    got=$(shasum -a 256 "$file" | cut -d' ' -f1)
  fi
  if [[ $got != "$sum" ]]; then
    echo "$name has SHA-256 $got, not the pinned $sum" >&2
    rm -f "$file"
    exit 1
  fi
  echo "$file"
}

kotlin() {
  generate kotlin
  local jna coroutines
  jna=$(jar net/java/dev/jna/jna/5.19.1 jna-5.19.1.jar \
    4fb141dd8ef6b0585ffceea4bc49602fbc6312fa977e2c488794ea3e6aafecae)
  coroutines=$(jar org/jetbrains/kotlinx/kotlinx-coroutines-core-jvm/1.11.0 \
    kotlinx-coroutines-core-jvm-1.11.0.jar \
    d1d75aa01dffbb4d1c520e67e4c4e7f5f6174718e7cb4632412503f2f0e604fa)
  local classpath=$jna:$coroutines
  kotlinc -nowarn -classpath "$classpath" -include-runtime -d "$out/kotlin/smoke.jar" \
    "$out/kotlin/racing/deadbear/wartui/core/wartui_ffi.kt" "$here/kotlin/Smoke.kt"
  java -Djna.library.path="$(dirname "$library")" -classpath "$out/kotlin/smoke.jar:$classpath" \
    SmokeKt
  echo "OK kotlin"
}

# The Swift written here needs Swift 6.4 and compiles in the Swift 6 language mode. The generated
# binding compiles in that mode too, and keeps UniFFI's own checks for older compilers.
swift_is_new_enough() {
  local numbers='s/.*Swift version \([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p'
  local major minor
  read -r major minor <<< "$(swiftc --version 2> /dev/null | sed -n "$numbers" | head -n 1)"
  if [[ -z ${major:-} || -z ${minor:-} ]]; then
    echo "could not read swiftc's version; the Swift smoke test needs Swift 6.4 or later" >&2
    exit 1
  fi
  if ((major < 6 || (major == 6 && minor < 4))); then
    echo "the Swift smoke test needs Swift 6.4 or later; swiftc is $major.$minor" >&2
    exit 1
  fi
}

swift() {
  swift_is_new_enough
  generate swift
  local dir
  dir=$(dirname "$library")
  swiftc -swift-version 6 -I "$out/swift" \
    -Xcc -fmodule-map-file="$out/swift/WartuiCoreFFI.modulemap" \
    -L "$dir" -lwartui_ffi -Xlinker -rpath -Xlinker "$dir" \
    -o "$out/swift/smoke" "$out/swift/WartuiCore.swift" "$here/swift/main.swift"
  "$out/swift/smoke"
  echo "OK swift"
}

case "$which" in
  kotlin) kotlin ;;
  swift) swift ;;
  all)
    kotlin
    swift
    ;;
esac
