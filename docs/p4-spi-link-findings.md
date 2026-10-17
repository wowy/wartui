# P4 ↔ C6 SPI link — can the on-board C6 feed the P4 fast enough

Measured on 2026-10-17 on Linux (Arch, kernel 7.2.11), on a Waveshare ESP32-P4-WIFI6-POE-ETH:
an ESP32-P4 rev v3.2 at 400 MHz (`8B:65`) and its on-board ESP32-C6 rev v0.2 (`D8:1C`). The
question was whether a split bridge is possible, with the C6 running the radio and the P4
buffering whole bursts in its RAM. That works only if the C6 can hand frames to the P4 faster
than one burst arrives. One node delivers 550–700 full batches a second inside a burst
(`link-encode-findings.md`), and two overlapping nodes deliver twice that.

Both firmwares came from branch `spike/p4-spi-link` (c91dd80, d4f82bd). The branch is never
merged. Both use esp-hal 1.2.2, and the C6 also runs esp-radio 1.0.0-beta.1. No `unsafe` was
needed in either.

## The setup

esp-hal has no SDIO slave driver for the C6. Instead, SPI runs over the six SDIO traces the
board already routes between the chips, through each chip's GPIO matrix:

| Signal | P4 GPIO | C6 GPIO |
| --- | --- | --- |
| SCLK, driven by the P4 | 18 | 19 |
| MOSI | 19 | 18 |
| MISO | 14 | 20 |
| CS, hardware, driven by the P4 | 15 | 21 |
| READY, driven by the C6 | 6 | 2 (the spare 0 Ω link) |

- The P4 is the SPI2 master, in mode 0 with DMA.
- The C6 is the SPI2 slave with DMA. esp-hal marks that driver unstable, and it does single
  transfers only.
- Every transaction is one full-duplex 1,036-byte transfer: a 10-byte header, a 1,024-byte
  payload and a CRC-16/CCITT-FALSE trailer.
- The C6 arms its DMA, raises READY, and drops READY once the transfer completes. It sends a
  stats frame every 2 s, otherwise any queued ESP-NOW frame, otherwise filler. So the link
  always runs flat out.
- The P4 sweeps the clock and holds each speed for 15 s. It checks every transfer's magic, CRC,
  sequence number and filler pattern. The C6 checks the P4's transfers the same way.
- The C6 stays on ESP-NOW receive on channel 6 the whole time, listening to the bench nodes'
  heartbeats: `F5:50` (ESP32-C5) and `9A:24` (a XIAO ESP32-C6 with an external antenna).

## It is error-free at every clock

The run covered two full sweeps and the start of a third: 2 min 45 s, 193,302 transfers counted
by the C6. Every error counter on both ends stayed at zero: CRC, magic, length, sequence gaps
and repeats, and filler pattern.

| Clock | Transfers/s | Payload | Time on the wire | Cycle | Overhead |
| --- | --- | --- | --- | --- | --- |
| 5 MHz | 503 | 513 KB/s | 1,658 µs | 1,988 µs | 330 µs |
| 10 MHz | 867 | 885 KB/s | 829 µs | 1,153 µs | 324 µs |
| 20 MHz | 1,359 | 1,389 KB/s | 414 µs | 736 µs | 322 µs |
| 26.67 MHz | 1,583 | 1,619 KB/s | 311 µs | 632 µs | 321 µs |
| 40 MHz | 1,897 | 1,940 KB/s | 207 µs | 527 µs | 320 µs |

The rates matched to the transfer across both sweeps. 40 MHz is the fastest clock tried, so the
link's upper limit wasn't found.

## The C6's own work sets the ceiling, not the wire

The overhead per transfer is a flat ~320 µs at every clock, so the C6's work between transfers
caps the rate, not the SPI clock.

The C6 counts `prep`: the time from one transfer completing to READY going high again. In that
time it verifies the P4's 1 KB transfer (a CRC and the filler pattern) and encodes its own next
one (a second CRC). `prep` averaged 243 µs (`prep_us_sum / prep_count`), with a maximum of
340 µs. The P4 saw the matching wait: READY rose 180 µs after it started waiting, at p50 and
p99. Its own verify ran while the C6 was still preparing.

The spike's C6 has one buffer pair, so it can prepare only between transfers. A production C6
would alternate two buffer pairs, filling one while the other transfers, and wouldn't verify
filler. That leaves the time on the wire as the limit: about 4,800 transfers/s at 40 MHz,
before the SPI start and stop costs, which weren't measured.

## Against the burst rate

- **One node,** 550–700 full batches/s: 10 MHz already carries it, at 867/s.
- **Two overlapping nodes,** 1,100–1,400/s: 20 MHz just reaches it at 1,359/s. 26.67 MHz
  carries it with room to spare, at 1,583/s.
- **Today's bridge** drains 364–608 frames/s (`link-encode-findings.md`). The 40 MHz link is
  about three times its best.

Even with the spike's naive single buffer, the link isn't the narrow point any more. The P4's
host link is: the CH343 at about 500 KB/s, or the P4's own USB.

## ESP-NOW receive ran alongside without loss

The C6 received 278 ESP-NOW frames during the run, all wartui frames (they start with
`air::MAGIC`). It forwarded each one as a kind-1 transfer. The forward count on the P4 tracked
the C6's receive count in every 15 s step, and no frame was dropped from the C6's 8-entry FIFO.
None of the SPI error counters moved while the radio was receiving.

This was only heartbeat traffic, about 1.7 frames/s. **Burst load wasn't tested.** A burst
needs the temporary burst build from `link-encode-findings.md`. The question that remains open
is whether the radio's receive interrupts stretch `prep` under a burst.

## The handshake recovery fires once per step

`ready_stale_recover` counted exactly 1 in every step after the first, and `ready_timeouts`
matched it. The cause is the P4's report itself.

1. The P4 prints the step's line at 115,200 baud, which blocks it for about 40 ms.
2. During that time the C6 drops READY and raises it again.
3. So the P4 misses the low it waits for.
4. It sits out the 100 ms timeout, then takes the still-high READY as a fresh edge.

That costs 0.7% of each step. Without the recovery (d4f82bd), the link would stop for good
after the first report. A production handshake should trigger on READY's rising edge with a GPIO
interrupt, rather than polling for low then high.

The first step's two timeouts and 88 ms worst wait are the C6 still booting after the P4
released its EN.

## Bench notes for this board

- **The P4 doesn't auto-reset into the bootloader from the CH343.** `espflash` connects only
  when BOOT is held while the board is plugged in. Its hard reset after flashing doesn't start
  the app either: press RST, or replug.
- **Opening the CH343 port can reset the P4** (`rst:0x1 (POWERON)`), even with DTR and RTS set
  low before `open()`. One RST press with the capture's port open landed in download mode
  (`boot:0x307`). The next, which was a replug, booted from flash.
- **The P4 moves to a new `/dev/ttyACM*` number when replugged.** Use
  `/dev/serial/by-id/usb-1a86_USB_Single_Serial_5C93090010-if00`.
- **The C6 flashes through header H7.** That needs IO9 jumpered to GND, the P4 held in its ROM
  by BOOT, and `espflash --before no-reset --after no-reset` on the USB-to-TTL adapter. Remove
  the jumper before the P4 next boots, because the P4 resets the C6 through EN.
- The images are 78 KB for the P4 and 525 KB for the C6.

## What this settles

- SPI over the SDIO traces, with esp-hal 1.2's C6 slave, is reliable at up to at least 40 MHz.
- It already runs faster than the burst rate of two overlapping nodes. Double-buffering should
  bring it near the wire's ~4,800 transfers/s.
- The split bridge is no longer limited by the C6-to-P4 link. The next questions are:
  - the P4's link to the host (the CH343 or its USB)
  - the C6 under burst receive load
