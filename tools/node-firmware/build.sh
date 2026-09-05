#!/usr/bin/env bash
#
# Build ESP32DualBandWardriver firmware for an ESP32-C5, in a chosen role.
#
# The role is a compile-time #define in configs.h with no override hook, so this
# copies the sketch to a build directory and edits the copy. Your checkout of
# the firmware repo is never modified.
#
#   ./build.sh                        # NODE role, from the current branch
#   ./build.sh --role core
#   ./build.sh --role node --upload /dev/cu.usbmodem14201
#
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="${WARDRIVER_REPO:-$HOME/code/ESP32DualBandWardriver}"
role="node"
upload=""
psram=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --role)   role="$2"; shift 2 ;;
    --repo)   repo="$2"; shift 2 ;;
    --upload) upload="$2"; shift 2 ;;
    --psram)  psram=1; shift ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

case "$role" in
  solo|core|node) ;;
  *) echo "role must be solo, core or node (got '$role')" >&2; exit 2 ;;
esac
[[ -f "$repo/src/src.ino" ]] || { echo "no firmware sketch at $repo/src/src.ino" >&2; exit 1; }

branch="$(git -C "$repo" rev-parse --abbrev-ref HEAD)"
commit="$(git -C "$repo" rev-parse --short HEAD)"
dirty=""
git -C "$repo" diff --quiet || dirty=" (uncommitted changes)"
echo "Firmware: $repo @ $branch $commit$dirty"
echo "Role:     $(printf %s "$role" | tr "[:lower:]" "[:upper:]")"

build="$here/build"
mkdir -p "$build/src"
rm -rf "$build/src"
cp -R "$repo/src" "$build/src"
cp "$here/platformio.ini" "$build/platformio.ini"

# Select the role in the copy. configs.h ships all three lines with exactly one
# uncommented, and #errors if that is not true.
python3 - "$build/src/configs.h" "$role" <<'PY'
import pathlib, sys
path, role = pathlib.Path(sys.argv[1]), sys.argv[2].upper()
text = path.read_text()
for name in ("SOLO", "CORE", "NODE"):
    wanted = f"#define {name}" if name == role else f"// #define {name}"
    for existing in (f"// #define {name}", f"#define {name}"):
        if existing in text:
            text = text.replace(existing, wanted, 1)
            break
    else:
        sys.exit(f"configs.h has no role line for {name}")
path.write_text(text)
PY
grep -A3 "//// Role stuff" "$build/src/configs.h" | sed 's/^/  /'

# Vendor the pinned libraries, matching the refs CI checks out. Cached between
# runs; delete build/lib to refresh.
mkdir -p "$build/lib"
vendor() {
  local dest="$build/lib/$(basename "$1")"
  [[ -d "$dest" ]] && return 0
  echo "  fetching $1@$2"
  git clone -q --depth 1 --branch "$2" "https://github.com/$1" "$dest"
}
vendor adafruit/Adafruit-GFX-Library 1.12.1
vendor adafruit/Adafruit-ST7735-Library 1.11.0
vendor adafruit/Adafruit_BusIO 1.15.0
vendor adafruit/Adafruit_MAX1704X 1.0.2
vendor bblanchon/ArduinoJson v6.18.2
vendor ivanseidel/LinkedList v1.3.3
vendor stevemarple/MicroNMEA v2.0.6
vendor h2zero/NimBLE-Arduino 2.3.0
vendor plerup/espsoftwareserial 8.1.0

if [[ "$psram" == 1 ]]; then
  # CI builds with PSRAM=enabled. Off by default here because enabling it
  # changes the flash/PSRAM pin configuration, and a board without the part
  # fitted may then fail to boot. It only feeds a display statistic.
  cat >> "$build/platformio.ini" <<'EOF'
board_build.psram_type = qio
board_build.arduino.memory_type = qio_qspi
build_flags = ${env:c5.build_flags} -DBOARD_HAS_PSRAM
EOF
  echo "  PSRAM enabled"
fi

cd "$build"
if [[ -n "$upload" ]]; then
  pio run -t upload --upload-port "$upload"
else
  pio run
  echo
  echo "Built: $build/.pio/build/c5/firmware.bin"
  echo "Flash with: $0 --role $role --upload /dev/cu.usbmodemXXXX"
fi
