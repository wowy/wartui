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

The drain rates are the result. The "Batches lost" column is not: loss depends on how the nodes'
bursts overlap, which drifts between runs (below).

## Under `wartui run`

Four minutes each with `wartui run`, a static position, and the bridge rate at 5 Hz. All
losses but a few dozen in each run were drops from the bridge's outbox. These runs do not show
what the change bought, for the reason in the next section:

| Bridge | Lost, `main` | Lost, changed |
| --- | --- | --- |
| Plain, run 1 | 1,579 (17.8%) | 443 (4.7%) |
| Plain, run 2 | 1,744 (19.6%) | 547 (6.2%) |
| Panel + GPS 5 Hz (`esp32c5,t-dongle-c5,t-dongle-c5-gps`) | 1,898 (21.6%) | 556 (6.4%) |

The bridge GPS supplied 99.9% of positions in both panel and GPS runs.

## Loss follows burst overlap, not the build

On 2026-10-17 the changed bridge, rebuilt unmodified from this branch, lost 1,729 batches
(19.5%) in the same 4-minute `run`, as many as `main` had. Nothing on the bridge or host had
changed. The nodes' timing had:

- Each node times its bursts by its own clock, from boot plus a per-node offset, and a burst
  that runs late restarts the period. So the four drift against each other.
- In a 2-minute `sniff` that lost 4.8%, `8B:90`'s and `C5:B8`'s bursts started 90–450 ms
  apart. In a later one that lost 18%, they started 10–70 ms apart, so two full bursts reached
  the bridge at once, at twice the rate.

So a loss count measures where the bursts fell as much as the bridge. The 17.8% to 4.7% above
compares runs a few hours apart and cannot be credited to the change. To compare bridge builds,
compare the drain rate, and alternate the builds run by run so they share the same overlap.

## Writing a packet's bytes in one call

Measured on 2026-10-17 with 2-minute `wartui sniff` runs on the T-Dongle-C5 bridge, timed with
`mcycle` on a temporary build. The outbox handed the sink one byte per call. Filling one 64-byte
packet that way took ~6,600 cycles at 240 MHz, ~100 a byte. About 36 of each byte's are the FIFO
status read `esp-hal` does before every write. The byte-per-call rows are the change above's own
runs from earlier that day, and the slice rows three runs from later.

| Bridge | Cycles per 64-byte fill | Drain once the radio is quiet | Drain while frames arrive |
| --- | --- | --- | --- |
| A byte per call | ~6,600 | 608–649/s | ~460–500/s |
| The rest of the frame as a slice | ~3,400–4,500 | ~716/s | 502–543/s |

The drain while frames arrive is noisy. No run had an undecodable frame.

A probe that read the status once per packet and wrote the FIFO register directly cut a fill to
~2,200 cycles but raised the quiet drain only to 733/s, about 2% more, in two pairs of runs
alternated with the slice build. It needs `unsafe`, so it was not adopted. Below ~4,000 cycles a
fill, the host's side of each packet sets the rate.

## What is left

- **The USB link itself.** Once the radio is quiet the drain is ~716 frames a second, about
  750 KB/s, and while frames arrive about 500–540. A node's burst arrives at 550–700 full
  batches a second, and two overlapping bursts at twice that, so a long enough burst still
  overflows the ring.
- **About 230 µs of encoding per full batch.** Serializing, checksumming and COBS-encoding in one
  pass, without the intermediate body buffer, was tried and bought nothing measurable.
- **This load is far beyond a drive.** The 2026-09-26 drive averaged 1.5–3 records a batch
  (`batch-loss-findings.md`).
