# USB boundary — frames held by a full packet

Measured on 2026-10-03: a HackberryPi CM5 host on Raspberry Pi OS, the C6 bridge `9D:24`,
and two nodes, `10:D0` and `8B:90`. A 29-minute capture ran with `--record-raw` and
`--log-file`. The question was why the host logged its link as behind when nothing else was
busy.

## Findings

- The log recorded 7 lag spells. Each was a pair of frames that reached the host 10–14 µs
  apart, though the bridge stamped them 0.77–3.0 s apart. The first of each pair was held
  until the second arrived.
- Every held frame was exactly 64 bytes on USB, the full-speed bulk max packet size. Those
  frames were sighting batches of 39 or 40 payload bytes, depending on whether the bridge's
  microsecond stamp took 4 or 5 varint bytes.
- All 8 frames in the capture that encoded to exactly 64 bytes were held. None of the 1,356
  others were.
- 7 were released by the next `Rx`, and 1 by a `Status` reply in the same millisecond.
  Whatever came next released it, so the cause is the transfer, not the frame.
- An earlier 25-minute run on the same host showed 12 spells, up to 3.78 s.

The USB-Serial-JTAG sends a packet by itself at the 64th byte, and a flush with an empty FIFO
sends nothing. A frame that ends on that boundary therefore ends its transfer with no short
packet. Linux `cdc_acm` reads with a buffer larger than 64 bytes, and that read completes
only on a short packet or a full buffer, so the frame waits for the next one.

## What it means

A held heartbeat arrives late and is marked replayed, so its admin window is missed. This is
a likely source of the "host behind" counts on earlier captures, such as the 42 replayed
heartbeats on the 2026-10-02 drive. How often it bites depends on the traffic: only a frame
whose encoding lands on exactly 64 bytes, at the end of what one pump wrote, is held.

## The change

The bridge's outbox counts the bytes written into the current USB packet. A pump that would
end on a packet boundary writes one lone `0x00` before it flushes, so the transfer ends on a
short packet. A lone `0x00` is an empty frame, which every receiver skips. If the FIFO is
full, the zero is owed and goes out first on the next pump.
→ `crates/wartui-proto/src/outbox.rs`

## The change on the bench

Measured on 2026-10-03 with the same host, bridge `9D:24` reflashed with the change, and the
same two nodes: an 11.5-minute capture with `--record-raw` and `--log-file`.

| | before | after |
| --- | --- | --- |
| frames of exactly 64 bytes on USB held until the next frame | 8 of 8 | 0 of 3 |
| other frames held | 0 of 1,356 | 0 of 564 |
| lag spells pairing frames microseconds apart | 7 in 29 min | none in 11.5 min |

- Each of the three 64-byte frames arrived on its own, 263–785 ms ahead of the next frame,
  and none logged a spell. Unchanged, all three would have: before the change every such
  frame was held.
- The one spell logged was the bridge's backlog at connect. The host attached 18.8 s after the
  bridge booted, and the buffered frames, stamped from 0.2 s of uptime, arrived together. That
  is what the lag estimate exists to catch.
- Nothing was lost: 570 frames received and 570 read, no batch gaps, and 141 of 141 heartbeats
  from each node.
