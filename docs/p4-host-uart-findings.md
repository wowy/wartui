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
