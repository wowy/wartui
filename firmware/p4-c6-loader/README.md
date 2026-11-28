# firmware/p4-c6-loader

An ESP-IDF app for a factory Waveshare ESP32-P4-WIFI6's P4. It lands
[`firmware/c6-loader`](../c6-loader/README.md) on the C6 through the OTA of ESP-Hosted, the
firmware the C6 ships with, over SDIO. It then resets the C6 into the C6 loader and relays the
C6 loader's verdict to the host. The design is [#395](https://github.com/wowy/wartui/issues/395).

It is C, not esp-hal, because ESP-Hosted's host side exists only as an ESP-IDF component.

## Flow

1. Wait 2 s for a listener, then send `start`.
2. Listen on the link UART (UART1, RX GPIO14, TX GPIO19, 115200 8N1) for 1.5 s. A
   `c6-loader: ` line means a C6 loader is already running, so skip to step 7. Otherwise free
   both pins, which are SDIO lines too.
3. Check the RTC guard (below), then start ESP-Hosted. Starting it resets the C6.
4. Connect to the C6. With no ESP-Hosted answering, send `skipped` and idle.
5. Ask for the C6's version, then push the embedded image with `ota_begin`, `ota_write` in
   1500-byte chunks and `ota_end`. `ota_activate` follows only on ESP-Hosted 2.6 or later; the
   factory C6 answers no version, so it goes without.
6. Stop ESP-Hosted and reset the C6 through GPIO54: low for 100 ms, then high.
7. Relay the C6 loader's lines for up to 120 s, until its verdict. Then idle.

The loader never restarts itself.

## What it reports

Lines on UART0, the console, which reaches the host through the CH343, at 115200. Each is a
whole line. `wartui flash-bridge` parses them.

| Line | Meaning |
| --- | --- |
| `wartui-loader: start` | the loader is running |
| `wartui-loader: hosted <x.y.z>` / `wartui-loader: hosted unknown` | the C6's ESP-Hosted version, or none answered (the factory C6) |
| `wartui-loader: sending <0–100>` | OTA progress, every 10 % |
| `wartui-loader: sent` | `ota_end` (and `ota_activate` on 2.6 or later) succeeded |
| `wartui-loader: c6 <text>` | each `c6-loader: <text>` line, relayed |
| `wartui-loader: done` | the C6 loader sent `done` or `done rehearsal` |
| `wartui-loader: failed <why>` | `ota_write at 123000: ESP_FAIL`, `c6 loader: <why>`, `no verdict from the C6 loader in 120 s` |
| `wartui-loader: skipped <why>` | `no ESP-Hosted on the C6` |

Every failed call reads `failed <call>: <error>`, as `ota_begin: ESP_ERR_TIMEOUT`. ESP-IDF's own
log is at warning level, so its lines are few and never start with `wartui-loader: `.

## Every reset reruns it

The loader lives in the P4's factory slot and runs on every boot. A rerun is safe in each state
the C6 can be in:

| The C6 runs | A rerun |
| --- | --- |
| ESP-Hosted | pushes the C6 loader again, as the first run did |
| the C6 loader | hears it in the 1.5 s listen and only relays, so the C6 is not reset under it |
| p4-radio | finds no ESP-Hosted and sends `skipped`, having written nothing |

A rehearsal, and a C6 loader that refuses its plan, both select ESP-Hosted again, so a rerun
after either starts over.

The listen hears a C6 loader only between its lines, and a step can outlast 1.5 s. A rerun that
misses it resets the C6 mid-step, which is a power cut at that step: the
[C6 loader's table](../c6-loader/README.md#order-of-writes) shows that only its bootloader and
table writes, a few sectors long, need the header.

**The RTC guard.** ESP-Hosted restarts the P4 when the C6 does not answer it. An `RTC_NOINIT`
word counts the attempts that reached ESP-Hosted. When the P4 boots from anything but a
power-on or the reset pin and an attempt is recorded, the loader sends
`skipped no ESP-Hosted on the C6 (the host restarted the P4)` and idles, so that restart cannot
loop.

## Building

`build.sh` needs ESP-IDF v5.5 exported (`. $IDF_PATH/export.sh`), and cargo and espflash on
the PATH.

```sh
./build.sh                                  # the C6 loader, then this app
C6_LOADER_FEATURES=rehearse ./build.sh      # with a rehearsal C6 loader inside
./build.sh c6                               # only main/c6-loader.bin
./build.sh p4                               # only this app, around the saved image
```

`c6` builds the C6 loader and saves its app image as `main/c6-loader.bin`. It refuses an image
over 1,572,864 bytes, the factory C6's `ota_1`, where ESP-Hosted's OTA lands it. `p4` builds
this app and merges it. The last line is the merged image's path,
`build/wartui-p4-c6-loader-esp32p4.bin`. It starts at 0x0 with the bootloader at 0x2000, so
write it at 0x0:

```sh
espflash write-bin --port <P4 port> --chip esp32p4 0x0 build/wartui-p4-c6-loader-esp32p4.bin
```

## Dependency versions

`main/idf_component.yml` pins `espressif/esp_hosted` and `espressif/esp_wifi_remote` exactly,
and `dependencies.lock` records the rest. ESP-Hosted's slave OTA and its SDIO pins are what
the bench proved against the factory C6, so a bump needs the bench again.
