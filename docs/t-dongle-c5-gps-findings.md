# T-Dongle-C5 GPS — what the receiver costs

Measured on 2026-10-16 on Linux (Fedora, kernel 7.2.10), against a LilyGO T-Dongle-C5
bridge (`B2:54`) with a SparkFun MAX-M10S (u-blox M10) on its Qwiic port. The bridge was
built `--features esp32c5,t-dongle-c5,t-dongle-c5-gps` unless a row says otherwise. The
first three sections had no nodes on the air.

The questions were:
- whether the Qwiic port is the I2C bus the schematic implies;
- whether the receiver holds 5 Hz over it;
- what reading it costs the bridge, and whether a fleet's traffic pays for it.

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

## What a blocking read costs the loop

Timed on a temporary build (not committed) of the blocking design, which read the
receiver from the main loop. `Instant::elapsed()` was put around `Receiver::poll`, polls
over 50 µs were counted, and each 10 s was reported as a `Log` frame:

| Receiver setting | Bytes/s | Busy per 10 s | Share of loop | Longest poll |
| --- | --- | --- | --- | --- |
| Factory: 1 Hz, every sentence | 1146 | 451 ms | 4.5% | 4.2 ms |
| 5 Hz, GGA/RMC every fix, GSA/GSV 1 Hz | 1586 | 583–608 ms | about 6% | 3.9 ms |

The 5 Hz row is four consecutive windows, steady.

A byte costs about 38 µs, 2.6 times the 14.4 µs a 400 kHz bus spends on it. The
difference is per-transaction overhead and the receiver stretching the clock. The longest
poll, a full 64-byte chunk, is under half a panel row (`t-dongle-c5-findings.md`), and an
eighth of an unacknowledged ESP-NOW send.

## Under node bursts, blocking reads cost batches

Measured later the same day with four nodes on the air: `C5:B8`, `DC:64` and `FA:04`
(ESP32-C5) and `8B:90` (a XIAO ESP32-C6 with an external antenna). Each ran a temporary
build (not committed) that sent bursts of 46, 91 and 132 sighting batches every 10 s,
each acknowledged by the bridge's MAC, on top of its ordinary sweep. That is the load
`t-dongle-c5-findings.md` used for the panel. Each run lasted 4 minutes, and builds
alternated so the room's traffic fell on all of them alike.

First, ordinary traffic with two nodes (about 0.6 frames a second) showed no difference.
Panel plus GPS at 5 Hz and panel alone both lost no batches in two 5-minute runs each.

Then the bursts, with batches of 3 records, about 70 bytes each. That is about 36 frames
and 108 records a second, close to the 45-byte batches of a real drive
(`batch-loss-findings.md`) but far faster. Batches lost per run:

| Bridge | Run 1 | Run 2 | Outbox drops |
| --- | --- | --- | --- |
| Panel only | 0 | 17 | 0, 0 |
| Panel + GPS 5 Hz, blocking reads | 38 | 26 | 9, 12 |
| GPS only 5 Hz, blocking reads | 12 | 3 | 3, 0 |
| GPS only 2 Hz, blocking reads | 2 | 5 | 0, 0 |
| Panel + GPS 5 Hz, reads deferred on busy passes | 2 | 4 | 0, 0 |
| GPS only 5 Hz, reads deferred on busy passes | 0 | 7 | 0, 0 |
| Panel + GPS 5 Hz, async reads on an interrupt executor | 20 | 23 | 0, 0 |
| GPS only 5 Hz, async reads on an interrupt executor | 4 | 0 | 4, 0 |

The two kinds of loss have different shapes.

- **Blocking reads lose short gaps spread through a burst.** They fall mostly on the
  fastest sender, `8B:90`, inside its 132-batch bursts. Every rise in the outbox's
  `dropped_tx` came within about 2.5 s of a cluster of them. While the receiver had a
  backlog (GSA and GSV, about 1 KB once a second), the loop read it back to back, 2.5–4 ms
  a read. That starved both the radio's ten-deep receive queue and the USB pump.
- **The panel loses one contiguous block a run.** The 17 in panel-only, and the 20 and 23
  with async GPS, were each a single gap on one node. That is the redraw-at-the-start-of-a-
  burst stall `t-dongle-c5-findings.md` describes, and it happens with or without GPS.
  With two runs a build, one block more or less decides the total, so the gap between the
  deferred and async panel rows is not a measured difference.

Deferring reads on busy passes and moving them to an interrupt executor both remove the
spread-out losses. The async design was kept: its reads cost the loop nothing while the
I2C hardware clocks the bytes, so it has no deferral window or backlog spacing to tune.
The bridge GPS supplied 99.7–100% of positions through every burst run.

## Full batches hit a ceiling that is not the GPS

The same bursts with every batch filled to the 1024-byte limit (35–36 records, about 1,300
records a second across the fleet) lost about a quarter of every run. Almost all of the
losses were drops from the bridge's outbox:

| Bridge | Lost, run 1 | Lost, run 2 |
| --- | --- | --- |
| Panel only | 2,974 | 2,984 |
| Panel + GPS 5 Hz, async | 3,210 | 3,365 |
| Panel + GPS 5 Hz, deferred | 3,227 | 3,391 |
| GPS only 5 Hz, async | 3,574 | 3,361 |
| GPS only 5 Hz, deferred | 2,987 | 3,071 |

In the first panel-only run, the bridge took in 8,835 frames, the host read 5,864, and
the outbox dropped 2,973. That is about 24 full frames a second reaching the host against
37 offered, with or without GPS. Under that saturation the GPS's own batches take bulk
slots, which may account for the 200–400 extra lost in some GPS runs. The bridge GPS still
supplied 99.7–99.8% of positions.

## Still unmeasured

- **Where the full-batch ceiling is.** `usb-link-findings.md` drained back-to-back 1 KB
  frames at about 670 KB/s on a XIAO C6 bridge, so the USB transfer alone does not
  explain about 25 KB/s. The candidates are the bridge's per-frame work and the host
  taking in 36 records a frame. The load is far beyond a drive.
- **3 and 4 Hz**, which differ from 2 and 5 only in arithmetic.
- **A USB GPS taking over** when the Qwiic cable is pulled mid-run.
