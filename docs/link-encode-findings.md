# Link encoding — why full batches overflowed the bridge

Measured on 2026-10-16 on Linux (Fedora, kernel 7.2.10), against a LilyGO T-Dongle-C5 bridge
(`B2:54`) built `--features esp32c5` unless a row says otherwise. The question was issue #202:
why a bridge lost about a quarter of full 1024-byte batches under bursts
(`t-dongle-c5-gps-findings.md` § "Full batches hit a ceiling that is not the GPS").

**Load**

Four nodes ran the temporary burst build from that document: `C5:B8`, `DC:64` and `FA:04`
(ESP32-C5) and `8B:90` (a XIAO ESP32-C6 with an external antenna). Each sent bursts of 46, 91
and 132 acknowledged sighting batches every 10 s, every batch filled to 35–36 records. The
build was never merged.

## The host is not the limit

`wartui sniff` has no engine, no store and no transmit. Under the same bursts it lost 2,194 of
8,631 batches in 4 minutes, 25%, as `wartui run` did. The host's reader is a dedicated thread
with a 4,096-event channel, so it never waits on the consumer.

## The limit is the burst, not the average

The 37 frames a second the fleet offered is an average over 10 s. Inside a burst, the bridge's
`rx_us` stamps show one node delivering 550–700 full batches a second, which is the USB link's
own rate or above it. The 24-slot bulk ring absorbs the difference only while the burst is
short:

- A 46-batch burst left about 16 frames queued, and lost none.
- 91- and 132-batch bursts filled the ring part-way in, which then evicted its oldest.

## Encoding a frame stops the USB pump

`sniff` output was stamped with the host's arrival time. A frame that reached the bridge before
the frame ahead of it reached the host was queued, and the spacing between such arrivals is the
drain rate:

| Bridge | Drain while frames arrive | Drain once the radio is quiet |
| --- | --- | --- |
| `main` | 364 frames/s | 608 frames/s |

A temporary build (not committed) timed the bridge's loop with `Instant` and reported every 2 s
as a `Log` frame. Forwarding a full batch took 476–667 µs, almost all of it in `outbox.send`,
against about 45 µs for a heartbeat. Encoding the frame alone, timed apart:

| Step, one full `Rx` | Time |
| --- | --- |
| `encode_frame` in all | ~515 µs |
| of which `crc16`, bitwise | ~310 µs |
| of which postcard, COBS and buffer setup | ~205 µs |

The main loop pumps no USB while it encodes. The IN FIFO holds one 64-byte packet, which the
host takes in about 90 µs, so the link sits idle for the rest of every encode. At 650 frames a
second that idle time is the drain lost.

## What each change bought

Each row adds to the one above. Measured with `sniff`, 2 minutes each with the timing build's
cheap timers in place, except the `main` row, which is the 4-minute `sniff` run without them:

| Bridge | Forward a full batch | Drain while frames arrive | Batches lost |
| --- | --- | --- | --- |
| `main` | ~540 µs | 364/s | 25% |
| Table-driven `crc16`, payloads written as byte strings | ~270 µs | 445/s | 11.7% |
| `opt-level = 3` for `wartui-proto`, `cobs` and `postcard` | ~230 µs | 497/s | 5.2% |

- **Byte strings are the same wire.** postcard writes a byte string and a byte sequence as the
  same varint length and the same bytes. A `main` host decoded every frame from the changed
  bridge, with none undecodable, and `crates/wartui-proto/src/link/bytes.rs` pins the equality.
- **The image grew 976 bytes** for all three, from `espflash save-image` at `esp32c5`.

## Under `wartui run`

Four minutes each with `wartui run`, a static position, and the bridge rate at 5 Hz. All
losses but a few dozen in each run were drops from the bridge's outbox:

| Bridge | Lost, `main` | Lost, changed |
| --- | --- | --- |
| Plain, run 1 | 1,579 (17.8%) | 443 (4.7%) |
| Plain, run 2 | 1,744 (19.6%) | 547 (6.2%) |
| Panel + GPS 5 Hz (`esp32c5,t-dongle-c5,t-dongle-c5-gps`) | 1,898 (21.6%) | 556 (6.4%) |

The bridge GPS supplied 99.9% of positions in both panel and GPS runs.

## What is left

- **The USB link itself.** Once the radio is quiet the drain is 608–631 frames a second, about
  640 KB/s, near the 670 KB/s `usb-link-findings.md` measured on a C6. A node can send full
  batches faster than that, so a long enough burst still overflows the ring.
- **About 230 µs of encoding per full batch.** Serializing, checksumming and COBS-encoding in one
  pass, without the intermediate body buffer, would cut more of it. Unmeasured.
- **This load is far beyond a drive.** The 2026-09-26 drive averaged 1.5–3 records a batch
  (`batch-loss-findings.md`).
