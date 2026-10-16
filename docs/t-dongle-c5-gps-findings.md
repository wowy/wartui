# T-Dongle-C5 GPS — what the receiver costs

Measured on 2026-10-16 on Linux (Fedora, kernel 7.2.10), against a LilyGO T-Dongle-C5
bridge (`B2:54`) with a SparkFun MAX-M10S (u-blox M10) on its Qwiic port. No nodes were
on the air. The bridge was built `--features esp32c5,t-dongle-c5,t-dongle-c5-gps` unless
a row says otherwise.

The questions were whether the Qwiic port is the I2C bus the schematic implies, whether
the receiver holds 5 Hz over it, and what reading it costs the bridge's loop.

## The pins are the schematic's

GPIO11 as SDA and GPIO12 as SCL, read off connector P3 in the vendor schematic, worked on
the first flash. `wartui sniff --verbose` showed NMEA from a receiver with a 12-satellite
fix. A build without `t-dongle-c5` streamed the same way and reported `no panel`.

## 5 Hz needs UART1 off

The first 5 Hz configuration set the rate and the I2C sentence set only. The receiver
held 5 Hz, with GGA every 0.2 s, but sent no GSV and one GSA in four, and printed
`$GNTXT,01,01,00,txbuf alloc*61` instead:

| In about 10 s | GGA | RMC | GSA | GSV | `txbuf` |
| --- | --- | --- | --- | --- | --- |
| Rate and I2C sentences only | 59 | 63 | 6 | 0 | yes |
| Also UART1 output off | 55 | 55 | 44 | 110 | none |

UART1 still sends NMEA at 9600 baud by default, about 960 B/s, and nothing on this board
reads it. The receiver's output buffers are shared across ports, so that backlog cost the
I2C stream its sentences. `ubx::valset_rate` turns UART1's UBX and NMEA output off. A
receiver that reports `txbuf` is logged once as dropping output.

2 Hz was checked the same way, from `[gps] bridge-rate-hz = 2`: GGA every 0.5 s.

## What the reads cost the loop

Timed on a temporary build (not committed), with `Instant::elapsed()` around
`Receiver::poll`. Polls over 50 µs were counted, and each 10 s was reported as a `Log`
frame:

| Receiver setting | Bytes/s | Busy per 10 s | Share of loop | Longest poll |
| --- | --- | --- | --- | --- |
| Factory: 1 Hz, every sentence | 1146 | 451 ms | 4.5% | 4.2 ms |
| 5 Hz, GGA/RMC every fix, GSA/GSV 1 Hz | 1586 | 583–608 ms | about 6% | 3.9 ms |

The 5 Hz row is four consecutive windows, steady.

A byte costs about 38 µs, 2.6 times the 14.4 µs a 400 kHz bus spends on it. The
difference is per-transaction overhead and the receiver stretching the clock. The longest
poll, a full 64-byte chunk, is under half a panel row (`t-dongle-c5-findings.md`), and an
eighth of an unacknowledged ESP-NOW send.

## Still unmeasured

- **Loss with nodes bursting.** The panel's lesson was that a blocking call at the start
  of a burst costs frames from the radio's ten-deep receive queue. A 3.9 ms read is a
  smaller stall than a redraw, but no node was on the air, so `dropped_tx` and batch gaps
  under load are unmeasured.
- **3 and 4 Hz**, which differ from 2 and 5 only in arithmetic.
