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

## At heartbeat load, ESP-NOW receive ran alongside without loss

The C6 received 278 ESP-NOW frames during the run, all wartui frames (they start with
`air::MAGIC`). It forwarded each one as a kind-1 transfer. The forward count on the P4 tracked
the C6's receive count in every 15 s step, and no frame was dropped from the C6's 8-entry FIFO.
None of the SPI error counters moved while the radio was receiving.

This was only heartbeat traffic, about 1.7 frames/s. The next section loads it with bursts.

## Under burst load, 26.67 MHz and up lost nothing

A second run, on the same day, loaded the C6 with real bursts.

**The nodes:** both ran the `burst-test` build, ported to esp-hal 1.2 on the unmerged branch
`bench/burst-load-1.2`. Each sends a burst of 46, 91 or 132 full sighting batches every 10 s,
990–1,023 bytes a frame, through the real acknowledged unicast path. Their offsets put both
nodes' bursts within a second of each other every time, so every burst was a two-node overlap.
That is the hard case.

**The C6:** a burst node sends only to its core. So the spike C6 broadcast a `ClearMsg` every
2 s (9ec9cca), and both nodes adopted it as core within a second of it booting. It sent 135
clears with no failures, through a `send_async` future it polls by hand, so the SPI loop never
waits on a transmit.

The run covered 4 min 28 s and just over three and a half sweeps. 4,819 batches were
acknowledged to the C6:

| Clock | Transfers/s, burst load | Transfers/s, heartbeats only | FIFO drops per 15 s step |
| --- | --- | --- | --- |
| 5 MHz | 491–497 | 503 | 81, 150, 39 |
| 10 MHz | 851 | 867 | 27, 22, 21, 21 |
| 20 MHz | 1,320 | 1,359 | 0, 7, 32, 2 |
| 26.67 MHz | 1,529–1,532 | 1,583 | 0, 0, 0 |
| 40 MHz | 1,820–1,822 | 1,897 | 0, 0, 0 |

The first 5 MHz step also dropped 115, but that step overlapped the C6's boot, so it is left
out.

- **Below 26.67 MHz, the C6's 8-entry FIFO overflows.** Two overlapping bursts arrive faster
  than the link drains them, as the burst rates predicted. At 20 MHz the link only just keeps
  up, and the FIFO overflows on some overlaps.
- **At 26.67 and 40 MHz, nothing was dropped** in any of six steps.
- **Every frame that entered the FIFO reached the P4.** By the last report, the C6 had taken
  in 5,181 frames and dropped 517 from the FIFO, leaving 4,664. The P4 counted 4,666 forwarded,
  and the difference is when each side took its snapshot.
- **The link stayed clean:** no CRC, magic, length, sequence or pattern error on either end,
  over about 308,000 transfers.
- **Receive interrupts cost a little:** `prep` rose from a 243 µs to a 264 µs average, and its
  maximum from 340 to 420 µs. Transfers/s fell 3–4%. The P4's p99 READY wait peaked at 350 µs,
  up from 190.

The radio's own receive queue is a separate loss point. It holds 10 frames, a private constant
in esp-radio 1.0.0-beta.1 (`RECEIVE_QUEUE_SIZE`, `esp_now/mod.rs:33`), and it drops its oldest
frame without a count. The C6 received 362 more frames than the nodes had acknowledged
batches, and those are its heartbeats. At the heartbeat-only run's 1.7 frames/s there would be
about 455. That shortfall of about 90 frames could be queue loss, or fewer heartbeats while
bursting. This bench can't separate the two; a sequence check per node on the P4 could.

On the air, the XIAO C6 (`9A:24`) missed 23 acknowledgements over the run (its seq ended 23 behind the C5's), its own losses
before the bridge. The C5 node (`F5:50`) missed none.

## The P4's high-speed USB port carries 7 MB/s to the host

A third run measured the P4's own link to the host.

**What has to change:** today's bridge, and the CH343 on this board's Type-C, top out at about
500 KB/s. The burst-load run above forwards 1.9 MB/s from the C6. So the P4 needs a faster way
out.

**The firmware:** `firmware/spike/p4-usb-hs` (5cfac1e, also on `spike/p4-spi-link`) runs the
P4's USB 2.0 OTG controller as a device, through esp-hal 1.2.2's `Usb::new_hs`. That uses the
internal UTMI high-speed PHY; no PHY or speed setting is needed. The firmware:

- presents one CDC-ACM interface through embassy-usb 0.6, with 512-byte bulk packets
- streams 1,036-byte records back to back once the host raises DTR: the same layout as the SPI
  link, with a sequence number and CRC, each encoded as it goes out
- needs no `unsafe` of its own (esp-hal's driver has some inside)

**The host:** `host/rate.py` reads the port through Linux's `cdc_acm` driver, the same path the
wartui host's serial reader takes. It checks every record.

**The result:**

- The device enumerated at high speed (`480M` in `lsusb -t`), behind three hubs.
- Two runs streamed **7.12 and 7.67 MB/s**, 30 s and 18 s long, so about 6,900–7,400 records/s.
  There were no CRC, pattern or sequence errors in 339,318 records.
- The P4's own console reported the same rate (7,672 KB/s per 5 s window).
- The first run's lower rate wasn't explained. The P4 had rebooted just before the second run.

That is about 15 times the CH343, and four times what the C6 link delivers at 40 MHz. The host
link is no longer the narrow point.

The limit wasn't found. High-speed bulk can carry far more than 7.7 MB/s, so the limit is likely
the driver's one-512-byte-packet-per-`await` write path, or the hubs. The rate is already
enough, so that wasn't pursued.

**The board's Type-A port is wired as a host port.** Its VBUS is `VBUS_OUT`, from a DIO7003
load switch (U2) whose enable is tied high (R2). So the board always drives 5 V onto it.

- An ordinary A-to-C cable from the Type-A to a PC ties the board's 5 V to the PC's. For this
  run, the VBUS pin of the cable's A plug was taped over with Kapton, which leaves only data and
  ground.
- The driver's default `vbus_detection: false` then treats the bus as always present.
- A production design needs a data-only cable, a board revision, or the Type-A as the board's
  only power source.

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
- **To run the app, replug the board's Type-C.** On three occasions, pressing RST landed the P4
  in download mode (`boot:0x307 … waiting for download`). Every replug booted it from flash.
- **The CH343's DTR and RTS don't reset the P4.** In the schematic, the CH343's RTS pin
  connects to nothing. Holding each of the four DTR/RTS combinations for 1.5 s drew no ROM
  output. Opening the port did coincide with a `rst:0x1 (POWERON)` twice, both times with the P4
  running an app, so expect a reboot when you open the console.
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
- With two nodes bursting at once, 26.67 MHz and above forwarded everything that left the
  radio's queue. Production should run at 40 MHz and give the C6 a FIFO deeper than 8.
- The P4's high-speed USB streams 7–7.7 MB/s to the host as CDC-ACM, four times what the C6
  link delivers, as long as VBUS is kept off the PC side of the cable.
- Neither link limits the split bridge any more. What remains:
  - whether esp-radio's 10-frame receive queue drops frames under a burst
  - the two links joined: C6 → SPI → P4 → USB, under burst load
