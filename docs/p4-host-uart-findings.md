# P4 host link over the Type-C — what UART0 through the CH343 carries

Measured on 2026-11-05 on Linux, on a Waveshare ESP32-P4-WIFI6: an ESP32-P4 rev v3.2
(`E8:F6:0A:E8:1A:D1`) and its C6 (`98:A3:16:81:96:C4`, `96:C4`). The board's Type-C reaches a
CH343 (`1a86:55d3`, USB full speed), which is wired to the P4's UART0 on GPIO37 (TX) and GPIO38
(RX). The bench answered three questions:
- Can the host link run there, and at what rate?
- Does the C6 link hold up on its FIFO once the P4's single UHCI moves to UART0?
- Does opening the port reset the board?

The harnesses were throwaway builds on top of `8b0fa13`, never merged.

## The setup

- UART0 through UHCI0 and `DMA_CH0`, no flow control. Two 4032-byte receive buffers, read the
  way `src/link.rs` reads the C6 link. Transmit packed frames into 4096-byte DMA writes. The C6
  was held in reset.
- The test frames carried a sequence number and a CRC-32, with payloads cycling through 40, 300,
  1000 and 1500 bytes.
- The host opened the port as wartui does: `serialport`, `FlowControl::None`, DTR and RTS never
  touched.
- Each rate ran 120 s of load both ways, then 120 s more with a flash op every 3 s. A flash op
  was a 4 KiB sector erase and write in the inactive app slot.

## 4 Mbaud is the fastest rate that stayed clean

| Rate (P4 actual) | host→P4 KB/s | P4→host KB/s | Errors without flash ops |
| --- | --- | --- | --- |
| 2 M (2,000,000) | 201.5 | 197.0 | all counters 0 |
| 3 M (3,004,694) | 299.7 | 293.3 | all counters 0 |
| 4 M (4,000,000) | 385.9 | 388.3 | all counters 0 |
| 5 M | — | — | the CH343 carries nothing |
| 6 M (5,981,308) | 444.9 | 574.4 | host→P4: 11 frames lost in 8 gaps, 9 FIFO overflows |

The CH343 carried nothing at 4.5, 4.8, 5.0 or 5.2 Mbaud: nothing reached the host, and host
writes stalled. The kernel reported each of those rates as accepted.

At 6 M the CH343 delivers in bursts with gaps between them. Each gap ends a UHCI read on an idle
line: 132,035 reads in 120 s, against 17,164 at 4 M. Before the next read is armed, the
128-byte FIFO overflows; it lasts about 213 µs at that rate.

The P4 measured the bytes the host sent nonstop on its own clock: about 1.99 M at 2 M, 2.96 M at
3 M and 3.81 M at 4 M. Above 2 M the CH343 delivers less than line rate. This measurement cannot
say whether it runs slow or leaves gaps between USB packets.

## The P4's own flash work loses host→P4 bytes

A flash erase and write took about 39 ms, and 44 ms at worst. Interrupts stay off for that
long, so the reader cannot arm its next DMA buffer. The armed 4032 bytes last about 10 ms at
4 M, and then the FIFO overflows. At every rate, 39 of the 40 ops lost host→P4 frames: about 11
per op at 2 M and 21 at 4 M. P4→host lost nothing; it paused.

## The C6 link held on its FIFO

The P4 read UART1 through esp-hal's async `UartRx` and `UartTx` instead of UHCI. Pins, 4 Mbaud,
the 64-byte FIFO threshold and the 96-byte RTS threshold were unchanged, and so was the C6's
firmware. The load was two C5 nodes on the burst-test build, in a 10-minute `wartui run`:

| | |
| --- | --- |
| C6→P4 bytes | 11,531,802 in 717,832 reads, longest 89 bytes |
| UART overflow, frame, parity and glitch errors | 0 |
| CRC, magic and UART errors, both ends | 0 |
| Longest P4 stall | 963 µs |
| C6 `radio-up` queue | 93% at its peak, 0 dropped |

No read reached the RTS threshold, so flow control probably never engaged. The load averaged
17.7 KB/s, and the busiest 10 s averaged 23.7 KB/s. This was real ESP-NOW burst traffic, not
filler run flat out, so it does not show how fast the FIFO path can go. The C6 also took a
570 KB image over the FIFO path at boot, with no link errors.

## Opening the port does not reset the board

The host opened and closed the CH343 20 times. The P4's uptime rose by about 1,004 ms per
1.005 s cycle and never fell.

## The bootloader talks on UART0

Every boot, the ROM banner and the second-stage bootloader's log go out at 115200 baud: about
250 ms of text, partition table included. A host listening at the link's rate reads it as
garbage.

## A Pi 5 host corrupts long bursts unless a hub sits between them

Measured on 2026-11-09 on a HackberryPi CM5 (Raspberry Pi OS, kernel `6.18.60+rpt-rpi-2712`,
kernel `cdc_acm`, the CH343 enumerated at full speed straight on an RP1 xHCI root port) against the
10-node carrier and the production bridge firmware, after a drive whose footer reached
`garbled 2,885` in 74 minutes with no node frame lost. A Raspberry Pi 5 (1 GB) then ran the same
test on each kind of port and showed the same fault on both: the same rate, the same frame lengths,
and the same damage shape. The same hub cured it there too.

| Host | Path | Run | Status bursts | Undecodable frames |
| --- | --- | --- | --- | --- |
| CM5 | straight into the Pi | 3 min, `sniff` | 30 | 111 |
| Pi 5 (1 GB) | straight into a USB 3.0 (blue) port | 5 min 39 s, `sniff` | 54 | 264 |
| Pi 5 (1 GB) | straight into a USB 2.0 (black) port | 3 min 35 s, `sniff` | 34 | 167 |
| Mac | straight into the Mac | 3 min, `sniff` | 37 | 0 |
| CM5 | through an unpowered USB 2.0 hub | 3 min 42 s, `sniff` | 45 | 0 |
| Pi 5 (1 GB) | through the same hub | 2 min 11 s, `sniff` | 27 | 0 |

What the hex dumps of the 111 showed, and the Pi 5's 264 and 167 repeated
(`RUST_LOG=wartui_bridge=debug` logs each `undecodable frame` with its bytes since this bench):

- Only long bursts were hit: the 512-byte `Gps` batches at 1 Hz and the ~1.3 KB `Log` burst every
  5 s. The node frames on the bench and the drive were short, 27-byte heartbeats and sighting
  batches averaging 36 bytes and never over 287, and about 29,000 of them came through intact with
  the bridge's counters showing nothing dropped. That is the drive's doing, not an immunity: a
  moving node rarely fills a batch, but a `Sightings` body runs to 1,024 bytes
  (`bus::SIGHTING_BATCH_MAX`), and a dense area's dozen access points with real SSIDs make a 300 to
  600 byte frame, the size band in which half the GPS batches failed. A sighting frame that fails
  this way is a lost batch (`batch_gap`), since the P4 acknowledged it to the node. The 74-minute
  drive lost 4 batches; a lost frame leaves no length behind, so whether those were its few longer
  ones is not known.
- Nothing was lost. Every garbled GPS frame had exactly the length a full batch encodes to (521
  bytes, 457 for the shorter second batch of an epoch) and its COBS code byte at offset 255 in
  place.
- A stretch of 29 to 55 bytes starting on a 64-byte boundary near the frame's start (offset 0, 64
  or 128) held a copy of the bytes from about 330 bytes later in the same burst, which were also
  present at their own place. So a short USB packet near the start of a burst was delivered with a
  later packet's payload. Hit at offset 0 the frame fails COBS, having lost its leading code byte;
  hit later it fails the CRC. When the copied bytes carried a later frame's `0x00`, one frame
  became two garbled chunks, which is why the log burst produced them in pairs.

A high-speed hub's transaction translator does the full-speed splitting instead of the RP1, and
the xHCI then sees ordinary high-speed bulk packets. The firmware needs no change: a shorter
`GPS_FRAME`, a capped DMA chunk or a lower `HOST_BAUD` would have been workarounds for the Pi's
path, and the hub removes the fault instead. Plugging the hub in hot rebooted the HackberryPi, so
plug it in before boot or with the Pi off.

Not measured: whether the fault is the RP1 silicon or the kernel's xHCI short-packet handling,
whether a powered hub or another kernel behaves differently, and whether a Pi 4 does the same.
