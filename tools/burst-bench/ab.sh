#!/bin/bash
# usage: ab.sh NAME FIRMWARE_DIR
#
# Flash the bridge from FIRMWARE_DIR (a checkout's `firmware/bridge`), sniff for 120 s and print
# its drain rates. Alternate two builds run by run, e.g. ab.sh m1 main/… ; ab.sh b1 branch/… ; …
#
# Environment:
#   BRIDGE    the bridge's MAC suffix (required, e.g. B2:54)
#   FEATURES  bridge features (default esp32c5)
#   WARTUI    the wartui binary to flash and sniff with (default: wartui on PATH)
#   OUT       where flash logs and sniff captures go (default: ./burst-bench-out)
#   SECS      sniff length in seconds (default 120)
set -u
: "${BRIDGE:?set BRIDGE to the bridge's MAC suffix}"
FEATURES=${FEATURES:-esp32c5}
WARTUI=${WARTUI:-wartui}
OUT=${OUT:-./burst-bench-out}
SECS=${SECS:-120}
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
"$WARTUI" flash-bridge --features "$FEATURES" --firmware-dir "$2" --bridge "$BRIDGE" \
    > "$OUT/$1.flash" 2>&1 || { echo "$1 flash failed"; tail -5 "$OUT/$1.flash"; exit 1; }
sleep 3
timeout -s INT "$SECS" "$WARTUI" sniff --verbose --bridge "$BRIDGE" 2>&1 \
    | python3 -u "$HERE/stamp.py" "$OUT/sniff-$1.txt" > /dev/null
echo -n "$1: "; python3 "$HERE/drain.py" "$OUT/sniff-$1.txt"
# The partial frame at the port opening counts as one; it is normal.
echo -n "  undecodable lines: "; grep -ci "undecodable\|bad frame\|checksum" "$OUT/sniff-$1.txt"
