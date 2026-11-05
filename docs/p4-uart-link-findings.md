# P4 ↔ C6 UART link — what UART1 through UHCI carries

Measured on 2026-11-05 on Linux, on a Waveshare ESP32-P4-WIFI6: an ESP32-P4 rev v3.2
(`E8:F6:0A:E8:1A:D1`) and its on-board ESP32-C6 (`98:A3:16:81:96:C4`, `96:C4`). The question was
whether the board's SDIO traces carry the bridge's link as a UART with DMA, at what rate, and
whether it holds under real and burst load.

The firmware is `1b77fd1` (transport) and `3f77492` (rename). The nodes are the C5s `F5:50` and
`4F:98`, on firmware 0.3.0.

## The setup

- UART1 on both chips, through esp-hal 1.2.2's `uart::uhci`. The wiring is in
  [`firmware/p4-bridge/README.md`](../firmware/p4-bridge/README.md#the-link-to-the-c6).
- FIFO full threshold 64 bytes, RTS threshold 96 bytes, DMA read buffers of 4032 bytes.
- Hardware flow control and UHCI are unstable esp-hal drivers. The driver's transmit wait carries
  an upstream comment calling its flush a workaround.

## The traces idle high

With the C6 held in reset, all four traces read high with no pull. The P4's pull-down reads them
low, and they read high again within 50 ms of releasing it. So something on the board pulls them
up. The value was not measured.

## 4 Mbaud is the fastest rate that stayed clean

The sweep ran on a throwaway harness that was never merged: a patched esp-hal copy adding only a
runtime baud setter and read-only status helpers. Each rate ran 120 s of bidirectional load, in
the real packet format through the real UHCI paths. Payloads mixed about 40, 300, 1000 and 2034
bytes. Wi-Fi and ESP-NOW receive were running, and `c6_espnow_rx` read about 238 frames per 120 s.
The byte counts are the harness's `p4_rx_bytes` and `c6_rx_bytes`.

| Rate | C6→P4 bytes | P4→C6 bytes | C6→P4 KB/s | Errors |
| --- | --- | --- | --- | --- |
| 2 M | 22,420,230 | 22,562,278 | 187 | all counters 0 |
| 3 M (actual 3,004,694 at both ends) | 32,963,980 | 33,173,508 | 275 | all counters 0 |
| 4 M | 42,988,134 | 43,210,818 | 358 | all counters 0 |
| 5 M | 52,443,976 | 50,941,296 | 437 | see below |

At 5 M, with UART error counters at 0:

| Direction | CRC failures | Seq gaps | Packets lost |
| --- | --- | --- | --- |
| P4→C6, at the C6 | 5,718 | 5,718 | 11,436 |
| C6→P4 | 7 | 7 | 11 |

The 5 M failure was not investigated. Hold phases at 5 M were clean, with traffic one way only.

The SPI link carried 1,940 KB/s of payload at 40 MHz
([`p4-spi-link-findings.md`](p4-spi-link-findings.md)). That was a different board and session, on
a link running filler flat out, so the comparison is only of scale: this link carries about a
fifth of that.

## The 10-minute soak at 4 Mbaud was clean

600 s of load, then 60 s of holds:

| | |
| --- | --- |
| C6→P4 packets | 254,540 |
| C6→P4 bytes | 214,704,490 |
| P4→C6 bytes, load | 216,502,832 |
| P4→C6 bytes, holds | 14,643,168 |
| CRC, magic, seq gap, UART and mismatch errors, both ends | 0 |
| Packets lost | 0 |
| `c6_espnow_rx` | 1,190 in load, 120 in holds |
| CTS-high samples during load | 4,097 |

So RTS engaged during load, and nothing was lost when it did.

## Flow control held the P4 back without stalling it

The C6 stopped reading for about 200 ms, 20 times per rate in the sweep (80) and 60 times in the
soak: 140 holds.

- Each stop's RX FIFO stopped at 96 or 97 bytes, all at the RTS threshold.
- No bytes were lost across a hold.
- The longest P4 write took about 202 ms, as expected.
- The longest single poll of `wait_for_done` was 22 µs, with zero polls above 2 ms.
- The P4's stall meter peaked at 1.7 ms, with zero gaps above 2 ms.
- The C6's peaked at 2.8 ms, with zero gaps above 10 ms.

Why the writers wait `FIFO_FILL` before `wait_for_done` is in
[`firmware/p4-bridge/src/link.rs`](../firmware/p4-bridge/src/link.rs).

**The limits of this test:**

- **The busy wait is unobserved, not ruled out.** `flush_last_byte`'s busy wait in esp-hal triggers
  only if a hold begins in about one byte time after a packet's last byte leaves the FIFO. The
  short-packet part of each hold raises the odds. A rough estimate is 3–8% per short-packet hold,
  and that estimate is not measured. It never showed.
- **The holds were stops of reading, not real flash erases**, which halt the CPU.
- **Only the C6 stopped reading**, so its transmitter was never deliberately held.
- **The harness reached each rate by a runtime switch.**

## Updates

**The bundled image crossed the link.** `flash-bridge` over USB, with a bundled C6 image of about
570 KB, updated the P4 and C6 together in 11 s and 17 s in two runs. Both were confirmed on trial,
with zero CRC, magic and UART errors. `seq_gap` read 1, the C6's own reboot.

**A C6 restart under a running P4 was seen only through that update.** The host saw the new life.
Whether the host re-adds peers after a C6 restart during a capture was not exercised on hardware.

**A C6 that needs another baud was not tested.** It is what the frozen core rules out.

**Over-the-air node updates through the bridge** carried about 900 KB at 41–43 KB/s, confirmed,
three times.

## A real capture lost nothing

10 minutes, two nodes on the desk, no GPS.

| | |
| --- | --- |
| Frames received | 426 |
| Frames dropped | 0 |
| Batches lost | 0 |
| Radio link packets | 545, with zero link errors and `seq_gap` 0 |
| Heartbeats missed | 1 of 240 (0.4%) |
| Assignments | 3, latency 2.13–2.54 ms |
| Peak `radio-up` | 242 of 98,304 bytes |

The POE-ETH SPI runs in [`p4-bridge-findings.md`](p4-bridge-findings.md) read 4.0–6.4 ms of
assignment latency and missed 11–29% of heartbeats. Those runs used a different board, nodes,
session and traffic, so the comparison suggests but does not show a cause. A later 40 s check run
here missed 1 of 16 heartbeats, so the 0.4% is a small sample.

## Bursts drained without loss, but did not truly overlap

Nodes ran the `burst-test` build: fabricated full batches, 46, 91 or 132 every 10 s.

| | One node, 180 s | Two nodes, 300 s |
| --- | --- | --- |
| Nodes | `F5:50`; `4F:98` joined on production firmware at about 82 s | `F5:50` and `4F:98` |
| Frames received | 1,696 | 5,614 |
| Frames dropped | 0 | 0 |
| Batches lost | 0 | 0 |
| Peak `radio-up` | 89,278 of 98,304 bytes (91%) | 91,331 bytes (93%) |
| Radio link | zero link errors | 5,665 packets, zero link errors |
| Heartbeats missed | — | 3 of 120 (2.5%) |
| Assignment latency | — | 1.99–4.98 ms |
| Host behind ≥100 ms | 12 times, worst 265 ms | 25 times, worst 268 ms |

**The two-node run is not a two-node overlap, and this is the most important caveat here.** Each
node sent 30 bursts of about 400 ms by arrival. 8 pairs overlapped only at their edges, and no
100 ms window held more than 30 sightings from both nodes. So a full two-node overlap was not
measured. Burst loss depends on that phase
([`link-encode-findings.md`](link-encode-findings.md) § "Loss follows burst overlap, not the
build"), so zero loss here says little about the overlap case.

**Inferred from the arithmetic, not measured:** one 132-batch burst is about 137 KB at about
700 batches/s, against a 358 KB/s drain. A single burst already peaks the queue above 90%. So a
full overlap is expected to overflow the 96 KB queue and evict its oldest frames.

A burst load like this is a bench load. The agreed bar is real drive load, about 1.5 KB/s for
20 nodes ([`batch-loss-findings.md`](batch-loss-findings.md) has the drive). The queue's margin
under bursts is thin.

## Bench notes for this board

- A board in ROM download mode showed `303a:0012`, and CH343 resets did not move it until a replug.
- `4F:98`, after running normally, refused `espflash` over its USB-JTAG twice
  (`Failed to connect`, `Invalid stub handshake`) and stopped heartbeating until replugged. The
  cause was not found. Updating it over the air worked each time.

## What this settles

- UART1 on the SDIO traces, through UHCI on both chips, runs clean at 4 Mbaud: zero errors at
  either end over a 600 s soak.
- 5 Mbaud does not stay clean under bidirectional load on this bench, and 4 Mbaud does.
- RTS and CTS held the sender across all 140 read stops with no bytes lost. The stall meters
  peaked at 1.7 ms on the P4 and 2.8 ms on the C6.
- The bundled C6 image crosses the link and is confirmed on trial, and node updates through the
  bridge work.
- At desk load with two nodes, a real capture had zero link errors and zero frames dropped.
- Non-overlapping bursts drain with zero loss, at 91–93% of the C6's queue. The margin is thin
  against bursts, and the agreed bar is drive load.

It does not settle:

- **A full two-node overlap.** Not measured, and expected to overflow the queue (inferred).
- **The busy wait in `flush_last_byte`.** Unobserved, not ruled out.
- **Real flash erases under transmit.** The holds stopped reading, and did not halt a CPU.
- **The C6's transmitter under a hold.** Only the C6 stopped reading.
- **A C6 restart during a capture.** Peer re-adds were not exercised on hardware.
- **Why 5 Mbaud fails.** Not investigated.
- **Whether the board explains the lower heartbeat loss.** Different board, nodes, session and
  traffic, and small samples here.
