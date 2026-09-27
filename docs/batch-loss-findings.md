# Batch loss — what a real drive actually lost

Measured on 2026-09-26: a 52-minute drive, six nodes, 23,496 frames. The question was
whether the batches `wartui` counts as lost (`crates/wartui/README.md` § "Reading the
fleet table", the `lost` column) are lost on the air or somewhere else, and what that
implied for how a node should send them.

| node       | batches | lost | loss | heartbeat loss | median link RSSI |
| ---------- | ------- | ---- | ---- | --------------- | ----------------- |
| 10D0 (BLE) | 1552    | 11   | 0.7% | 0.4%             | -41                |
| 4F98       | 2490    | 55   | 2.2% | 1.1%             | -46                |
| 5784       | 1371    | 100  | 6.8% | 5.5%             | -51                |
| 5950       | 627     | 33   | 5.0% | 3.4%             | -52                |
| 4F08       | 1070    | 7    | 0.6% | 0.6%             | -43                |
| C5B8       | 957     | 44   | 4.4% | 5.7%             | -48                |

## Findings

- 250 batches lost across the fleet, 3.0%, about 420 records at 1.7 records/batch —
  the drive's whole loss, at the batch sizes below.
- Heartbeat loss tracks batch loss per node, so frame size does not matter: this is
  loss the channel imposes on any frame in the room, not something the sighting
  batch's extra bytes make worse.
- A loss was no more likely when the bridge was busy: a median of 3 frames in the
  200 ms before a loss against a baseline of 2. The bridge's own load is not the
  cause.
- Losses on different nodes do not line up: 262 had another node's loss within 2 s,
  against about 294 expected by chance. Not the bridge's receive queue or its USB
  path, both of which would correlate losses across nodes.
- 244 of the 250 gaps were one batch long. The per-minute rate varied from 0 to 29
  over the drive, and RSSI dipped only about 3 dB around a loss. This is collisions
  or interference on control channel 6, not a node falling out of range.
- Batches average about 45 bytes and 1.5–3 records.

## What it means

Every lost batch muted its networks for `DEDUP_REFRESH_MS` — five minutes. The host
counted the gap, but the node recorded a broadcast as reported once it was on the air,
and had no way to learn that nobody heard it. Unicasting a sighting batch to
the bridge puts the radio's own retry behind it and turns "lost" into "unacknowledged,
so recorded as reported only on ack": the addresses in an unacknowledged batch stay
due and go out again the next time the node hears them, rather than going dark for a
whole refresh window.

## Open questions

- The per-failure retry cost with the bridge absent is not yet measured; phase 0 saw
  31 retries in that state.
- The next drive should show `batches_lost` near zero. `seq` now advances only on an
  ack, so the count holds only loss after the bridge's radio took the frame, and the
  findings above found none of that. The node's `unacked` count in its log lines is
  where the air loss shows now.
- The Bluetooth node's reception of acks right after a scan is unmeasured — its
  antenna is shared with the controller, and a unicast's wait falls right after a
  sweep.
