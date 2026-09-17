# T-Dongle C5 ideas

Notes toward running the bridge on a LilyGO T-Dongle C5 and using its display.
Nothing here has been measured: the figures are estimates read off
`firmware/bridge/src/main.rs` and the traffic rates in
[`phase-4-findings.md`](phase-4-findings.md). Treat them as the case for a bench,
not the result of one.

## The board

From [Xinyuan-LilyGO/T-Dongle-C5](https://github.com/Xinyuan-LilyGO/T-Dongle-C5):

| Part | Detail |
| --- | --- |
| SoC | ESP32-C5HR8 — single-core RISC-V, 240 MHz, 2.4 and 5 GHz |
| Flash / PSRAM | 16 MB / 8 MB |
| Display | ST7735, 0.96", 80×160, SPI — MOSI 2, SCK 6, MISO 7, CS 10, RS (D/C) 3, RST 1, BL 0 |
| microSD | SPI, sharing MOSI 2 / SCK 6 / MISO 7 with the display, CS 23 |
| LED | APA102 — CI 4, DI 5 |
| USB | USB-A on the native USB pins (DN 13, DP 14), so `UsbSerialJtag` as today |
| Other | BOOT on GPIO 28, UART0 on 11 (TX) / 12 (RX) |

## How much CPU the bridge uses

Almost certainly low single-digit percent. The main loop is one thread that
moves radio frames into the outbox, reads at most `USB_READ_BUDGET` (256) bytes
from the host, pumps COBS bytes into the USB FIFO, and sleeps `IDLE_SLEEP` (1 ms)
when none of that did anything.

The per-frame work is a copy of at most 250 bytes, a COBS encode and a
timestamp — microseconds at 240 MHz. Phase 4 measured roughly 1–2.5 frames/s
per node, so even a full twenty-node fleet is on the order of 50 frames/s. The
largest steady cost is probably the 1 kHz idle wake-up and the Wi-Fi blobs' own
tasks, not anything wartui does.

None of the ways this bridge has been seen to fall behind are CPU-bound:

- the USB transmit endpoint that stops draining (`TxStalled`,
  [`firmware/bridge/README.md`](../firmware/bridge/README.md));
- the radio's receive queue, ten frames deep, dropping its oldest;
- the blocking wait for a transmit callback, 2–35 ms per send.

**To actually measure it:** accumulate the time spent in `IDLE_SLEEP` against
wall time and report the ratio in `Status`, so `wartui status` shows it. That
is a small change but a `Status` wire change.

## Would an S3 be better?

Not for CPU. It is dual-core, but the bridge runs a single thread, and none of
the limits above depend on how fast the core is. The S3's case is that it is
cheap and loses nothing by lacking 5 GHz. Against it: the `espup` / `+esp`
toolchain, and no `CpuLockup` reset cause — the C5 is the only supported part
that reports one ([`phase-3-findings.md`](phase-3-findings.md)).

## Is there room for the display?

Yes, in CPU and RAM. The constraint is not blocking the loop.

**RAM.** An RGB565 framebuffer is 80 × 160 × 2 = 25.6 KB. The C5 has about
384 KB of internal SRAM, so it fits without touching PSRAM. Keep it in a
`static` rather than on the heap: the bridge's heap is 100 KiB and belongs to
the radio blobs (an S3 bridge reported about 55 KB of it free). Drawing straight
to panel regions with `embedded-graphics` and `mipidsi`, which supports the
ST7735, avoids the framebuffer altogether.

**Time.** Rasterising a few lines of text is well under a millisecond. The SPI
push is what costs: a full frame at 20–40 MHz is roughly 6–15 ms of blocking
transfer inside the loop. Against a ten-frame receive queue and a 100 ms admin
window that is acceptable if the display:

- refreshes at 1–2 Hz, never continuously;
- redraws only the fields that changed;
- draws only when the loop would otherwise have slept, never while a transmit
  is pending.

DMA-backed SPI shortens the full-frame case further.

**Pins.** The display and the microSD slot share one SPI bus, which only matters
if the SD card is ever used. USB stays on the native pins the bridge already
uses. No conflicts with anything the firmware touches today.

## What it would show

This is the real design question. The bridge is format-blind by design, so on
its own it can only show what it already knows:

- channel, receive count, `dropped_tx`;
- whether a host is attached (the same signal `StallWatch` uses);
- peer-table occupancy.

Anything about the fleet — nodes up, sightings, GPS fix — needs one of:

1. **A display frame from the host** down the existing link. A `wartui-proto`
   link change, but it keeps logic on the host where it is testable. Preferred.
2. **Teaching the bridge to parse air frames.** Breaks the "no protocol
   knowledge" rule and turns display fixes into reflashes. Avoid.

## Cheaper alternative

The APA102 LED alone covers the at-a-glance case — linked, receiving, stalled —
with no SPI bus, no framebuffer and no blocking transfer.
