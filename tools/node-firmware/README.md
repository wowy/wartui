# Building node firmware

Builds [ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver)
for an ESP32-C5 in a chosen role, so nodes can be put on a specific branch
rather than whatever the web installer last shipped.

```sh
./build.sh                                          # NODE role, current branch
./build.sh --role core
./build.sh --role node --upload /dev/cu.usbmodem14201
```

Point it elsewhere with `--repo PATH` or `WARDRIVER_REPO`; it defaults to
`~/code/ESP32DualBandWardriver`. It prints the branch and commit it is building
so there is no doubt what ended up on the device.

## Why this exists rather than arduino-cli

Upstream builds with `arduino-cli` in CI and ships no PlatformIO project. That
works, but means a second toolchain and roughly 2 GB of ESP32 core download
alongside the one wartui already needs. This reproduces the same build with the
PlatformIO toolchain that is already installed.

Everything is pinned to what
`.github/workflows/wardriver_build_parallel.yml` uses: arduino-esp32 3.x, the
`min_spiffs` partition scheme, `-DC5_WARDRIVER`, and the exact library refs it
checks out.

## Things that are not obvious

**The role is a compile-time `#define`.** `configs.h` has `SOLO`, `CORE` and
`NODE` with exactly one uncommented, and `#error`s otherwise, so it cannot be
selected with a `-D` flag — defining a second role trips the mutual-exclusion
check. The script copies the sketch to `build/` and edits the copy; your
checkout of the firmware repo is never modified.

**Libraries are vendored into `build/lib`, not declared as `lib_deps`.** The
Adafruit ST7735 package declares `depends=Adafruit GFX Library, Adafruit seesaw
Library, SD`, and that `SD` resolves to the AVR `arduino-libraries/SD`, which
shadows the ESP32 core's own SD and then fails to compile. Vendoring stops
PlatformIO resolving transitive dependencies at all, which is also closer to
what CI does. They are cached; delete `build/lib` to refetch.

**Three extra include paths.** The core's SD library includes `ff.h`,
`diskio_impl.h` and friends from the IDF FATFS component, which are not on the
include path by default under PlatformIO.

**`-Wl,-zmuldefs` is required.** Upstream CI patches this into `platform.txt`;
the sketch has duplicate symbols across translation units and does not link
without it.

**Warnings are not errors here.** CI compiles with `--warnings none`. This
codebase does not build warning-clean, and pioarduino promotes warnings to
errors by default.

**PSRAM is off by default, unlike CI.** Enabling it changes the flash and PSRAM
pin configuration, so a board without the part fitted may fail to boot. In this
firmware it only feeds a display statistic. Pass `--psram` to match CI exactly.

## Flashing

`--upload PORT` hands off to `pio run -t upload`. Find the port with
`pio device list` or `ls /dev/cu.*`. Use the `/dev/cu.*` device, never
`/dev/tty.*`, which blocks waiting on carrier detect.

The resulting layout matches the vendor flasher: bootloader at `0x2000`,
partition table at `0x8000`, OTA data at `0xe000`, application at `0x10000`.

## After flashing a node

Set it up through its web UI — it raises an AP called `c5wardriver`
(password `c5wardriver`) at `http://192.168.4.1` when it cannot join a known
network. **wartui needs `Use Encryption` switched off**; a node with it on
unicasts to the core and a bridge cannot hear it at all.
