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

## Open

- The change has not been measured on hardware yet. A capture on the CM5 host should show
  no 64-byte frame held, and no lag spell that pairs frames microseconds apart.
