# C6 first flash over SDIO — a new P4-WIFI6 without header H4

Measured on 2026-11-27 and 2026-11-28 on `A9:76`, a factory-fresh Waveshare ESP32-P4-WIFI6, from
a Mac. Issue #395 asked whether `wartui flash-bridge` can take a board from factory to bridge over
the one Type-C cable, without soldering H4 on the C6. The design is in the issue. The code is
[`firmware/p4-c6-loader`](../firmware/p4-c6-loader/README.md) (the P4 loader),
[`firmware/c6-loader`](../firmware/c6-loader/README.md) (the C6 loader) and
`crates/wartui/src/flash/setup.rs`.

## The probe, 2026-11-27

Throwaway probes, not committed.

**The P4 cannot put the C6 into download mode.** It reaches the C6's SDIO lines (P4 14–19 to C6
18–23) and CHIP_PU (P4 GPIO54), not the C6's IO9 or UART0. ESP-Hosted, which the C6 ships with,
is the only way in.

**The factory P4.**

- Rev v3.2, 32 MB of flash, ROM `esp32p4-eco7-20260109`.
- Its app is Waveshare's `esp-brookesia` demo (ESP-IDF v5.5.4), which links the `esp_hosted` SDIO
  host. Its table puts a 9 MB factory app at 0x110000.
- Opening the CH343 does not reset it, and a reset prints only the ROM's lines on UART0.

**The factory C6.**

| Partition | Offset   | Size     |
|-----------|----------|----------|
| nvs       | 0x9000   | 0x4000   |
| otadata   | 0xd000   | 0x2000   |
| phy_init  | 0xf000   | 0x1000   |
| ota_0     | 0x10000  | 0x180000 |
| ota_1     | 0x190000 | 0x180000 |

- `ota_0` runs ESP-Hosted's `network_adapter`, `release/ng-v1.0.2-330-g83efce6`, built 2024-10-18
  on IDF v5.4-dev. `otadata` held one entry, seq 1 (`ota_0`), state VALID. The bootloader is IDF
  v5.4-dev, on 4 MB of flash.
- It does not answer `GetCoprocessorFwVersion`: RPC 0x15e times out after 1 s. No version can be
  read, so `activate` cannot be gated on one.
- `ota_begin`, `ota_write` (1500-byte chunks) and `ota_end` from an `esp_hosted` 2.12.13 host on
  IDF v5.5.4 took an 82 KB esp-hal image. `ota_end` wrote `otadata` seq 2 (`ota_1`) with state
  0xFFFFFFFF, so this bootloader does no rollback.
- After a reset through GPIO54, the esp-hal image booted from 0x190000 under the factory
  bootloader. It reported over UART1 TX GPIO20 to P4 GPIO14 at 115200.
- That image erased the newest `otadata` sector, and the next reset ran ESP-Hosted again. The
  probe ran twice, and the second run found ESP-Hosted back in place. The round trip can be
  undone, and so can any image ESP-Hosted takes.

**Our C6 table against the factory one.** `nvs`, `otadata` and `phy_init` sit at the same
offsets. Our `ota_0` is 0x10000 + 0x1E0000, `ota_1` 0x1F0000 + 0x1E0000 and `trial` 0x3D0000. The
C6 loader lands in the factory `ota_1` at 0x190000, which is inside our `ota_0`.

## The rehearsal, 2026-11-28

Both loaders were built and run on `A9:76` with the C6 loader in rehearsal: it plans and reports
every write and makes none. The embedded p4-radio app is 658,208 bytes. The C6 loader's own
image is 758,240 bytes. The factory C6 stays in place throughout, so each run is repeatable.

### The C6 loader through a probe P4

A probe P4 program pushed the C6 loader through ESP-Hosted and listened on UART1. Both runs
ended `done rehearsal`.

| Step                  | Run 2, s |
|-----------------------|---------:|
| `esp_hosted_init`     |     3.43 |
| `connect_to_slave`    |     5.30 |
| `ota_begin`           |     8.43 |
| `ota_write` done      |    15.08 |
| `ota_end`             |    15.30 |
| C6 reset              |    15.91 |
| C6 loader's `start`   |    16.27 |
| C6 loader's `done`    |    16.92 |

The OTA of 758,240 bytes at 1500-byte chunks took 6.8 s, from `ota_begin` to `ota_end`. The
probe's two `get` calls each waited 1 s for an answer the factory C6 does not give.

The C6 loader planned these spans, and the same set came back each run:

| Step                                      | Span              |
|-------------------------------------------|-------------------|
| Write and check p4-radio's app            | `0x10000..0xb1000` |
| Erase `trial`                             | `0x3d0000..0x3d1000` |
| Write the bootloader                      | `0x0..0x8000`     |
| Write the partition table                 | `0x8000..0x9000`  |
| Erase both `otadata` sectors              | `0xd000..0xf000`  |
| Erase the factory's newest `otadata` entry | `0xe000..0xf000`  |

### The P4 loader

**The first build failed.** It installed UART1 on P4 GPIO14 and GPIO19, SDIO D0 and CMD, before
`esp_hosted_init`, to listen for a C6 loader that might already be running, then ran
`gpio_reset_pin` on them. ESP-Hosted sets up its SDIO routing before `app_main`, and that broke
it. Runs 1 and 2 were identical:

```
 6.81 E (10379) H_SDIO_DRV: card init failed
 6.81 wartui-loader: hosted unknown
21.82 W (25404) rpc_core: Timeout waiting for Resp for [0x110](Req_OTABegin)
21.82 wartui-loader: failed ota_begin: ESP_FAIL
```

The log also held `sdmmc_io_reset: unexpected return: 0x108`. `ota_begin` timed out after 15 s.

**A diagnostic build with only that pre-listen compiled out ran the whole flow.** So the loader
keeps off GPIO14 and GPIO19 until `esp_hosted_deinit`, and cannot listen for a C6 loader before
it connects.

**`esp_hosted_connect_to_slave` returns `ESP_OK` when the link never comes up.** In esp_hosted
2.12.13 it only starts the transport (`host/api/src/esp_hosted_api.c:179-191`). The loader waits
up to 5 s for `ESP_HOSTED_EVENT_TRANSPORT_UP` instead. This answers part of #395's open question
about a C6 with no ESP-Hosted: after a failed card init, no P4 restart was seen within 21 s.

**The factory C6 gives `hosted unknown`.** `GetCoprocessorFwVersion` times out at 0x15e.

Runs 3 and 4 passed, and ran the same. Run 4, in seconds from the loader's start:

| Event                     | Run 4 |
|---------------------------|------:|
| `start`                   |  2.28 |
| `hosted unknown`          |  5.14 |
| `sending 0`               |  6.23 |
| `sending 100`             | 12.85 |
| `sent`                    | 13.06 |
| C6 loader's `c6 start`    | 13.94 |
| C6 loader's `c6 done`     | 14.55 |
| `done`                    | 14.55 |

The whole rehearsal runs in 14.6 s from the P4 loader's start. The loader prints progress every
10 %, 0.65 s apart.

### The setup from `flash-bridge`

`flash-bridge --first-flash --loader <the rehearsal loader>` on `A9:76`:

```
A9:76  writing the loader
A9:76  the loader is running
A9:76  the C6 runs ESP-Hosted, which names no version
A9:76    0%  sending the C6 loader
…
A9:76  100%  sending the C6 loader
A9:76  the C6 loader is sent; the C6 runs it
C6     start
C6     step would write the radio image at 0x10000..0xb1000
…
C6     done rehearsal
A9:76  the C6 is set up; writing the bridge firmware
waiting for the bridge to answer with its C6's address
Error: … runs the bridge firmware, but its C6 is not answering, so the bridge is not remembered.
```

The error is the intended outcome of a rehearsal. The C6 still runs ESP-Hosted, which does not
speak the bridge's link, so the run waits 90 s and fails without saving the board. Afterwards the
P4's bridge answers with the P4's own address, not the C6's.

### The write test

An earlier rehearsal, with the loader's write test on, proved the write, verify and hash path on
silicon. The test copies went to free flash at 0x310000..0x3ba000, below our trial. The radio
image's write and hash took 1.7 s, the bootloader 0.08 s and the table 0.02 s.

## Bench practice

Never hold a reader open on the CH343 while espflash runs on it. Both share the tty's bytes: the
reader eats espflash's replies, and espflash hangs with the P4 parked in download mode. Read the
port before or after, never during.

## End-to-end, 2026-11-28

### Preparation

The P4 was blanked with `espflash erase-region --chip esp32p4 --port <P4> --non-interactive 0x0
0x10000`, so it sat silent in ROM, and `flash-bridge --dry-run` offered it. The plain P4 loader
was confirmed to embed the same p4-radio app the P4 bridge bundles (SHA-256 `55af9c36…`, 658,208
bytes), with no rehearsal code in it.

### The setup

`flash-bridge --config <scratch> --loader <the plain P4 loader>` on `A9:76`, answered `y` at the
prompt. The loader came up and the C6 ran ESP-Hosted, which named no version. The C6 loader was
sent and ran all six steps:

```
C6     start
C6     step write the radio image at 0x10000..0xb1000
C6     step check the radio image at 0x10000..0xb1000
C6     step erase trial at 0x3d0000..0x3d1000
C6     step write the bootloader at 0x0..0x8000
C6     step write the partition table at 0x8000..0x9000
C6     step erase otadata at 0xd000..0xf000
C6     done
A9:76  the C6 is set up; writing the bridge firmware
9D:6C  set up; the bridge is known by its C6's address
```

The bridge answered with its C6's address, `9D:6C`, and was remembered. The exit status was 0.

### Afterwards

- `wartui ports` lists the P4 bridge as `…:9D:6C`.
- `wartui status` shows the radio `…:9D:6C` on firmware 0.1.0 with a clean radio link: 27 packets,
  no CRC, magic or sequence errors, and no "updating the radio". The bundle digest matched, so the
  P4 did not reflash the C6.
- The only faults are the bare board's: no carrier and no GPS.
- A second `flash-bridge` said `9D:6C already runs this image`.

### The `skipped` path

The P4 loader was written by hand while the C6 ran p4-radio, so the C6 had no ESP-Hosted:

```
   2.28 wartui-loader: start
   2.28 W (4362) H_SDIO_DRV: Reset slave using GPIO[54]
   5.74 E (7787) H_SDIO_DRV: card init failed
  35.74 wartui-loader: skipped no ESP-Hosted on the C6
```

`card init failed` came about 3.5 s after the reset, and the P4 did not restart within 70 s. The
`skipped` line came at 35.7 s: the 5 s transport wait plus the 25 s listen for a running C6
loader. `flash-bridge --bridge <path> --first-flash` then printed "the C6 loader was skipped: no
ESP-Hosted on the C6; writing the bridge firmware", and the bridge came up with C6 `9D:6C`. The
config was left unchanged, since a board named by `--bridge` is not remembered.

This answers #395's open question: `esp_hosted_connect_to_slave` neither waits long nor restarts
the P4 against a C6 without ESP-Hosted. It returns, the transport event never comes, and card init
fails within about 3.5 s.

### Open

Other boards may ship another ESP-Hosted version. Only `release/ng-v1.0.2-330-g83efce6` has been
met.
