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

## The change on the bench

Measured on 2026-09-27 with the unicast build on bridge `0A:28` and the same six C5
nodes, all on one desk. Every frame was stored with `--record-raw`, and each node's
serial log was read beside it. Three runs:

| run | length | batches | lost | unacked | node-logged vs stored records |
| --- | --- | --- | --- | --- | --- |
| Wi-Fi only | 10 min | 114 | 0 | 0 | equal on every node |
| `10:D0` given the Bluetooth scan | 3.5 min | 86 | 0 | 0 | equal on every node; 105 of 105 on `10:D0` |
| bridge unplugged for 137 s | 4 min | 96 | 0 | 65 during the outage | equal, or one more stored |

- Every node logged `core is now 10:BD:A3:CC:0A:28` with its first assignment. A later
  run against the same bridge logged no new core and re-sent nothing, as it should.
- The Bluetooth node's acks arrive. It sent 185 scan reports and 40 batches straight
  after its scans, and none went unacknowledged.
- An absent bridge costs the sweep nothing measurable. The heartbeat counter still
  counts sweeps while nobody hears them, so its jump across the outage gives the sweep
  rate without a bridge: 979, 1047, 1006, 1089, 1031 and 1069 ms per sweep, against
  978, 1046, 1005, 1089, 1027 and 1068 ms with one. The failures were sparse (3, 6, 8
  and 48 on the four nodes that had anything new to send; none on the other two),
  because the dedup ring had already muted most of the room, and the one failed frame
  per report is the most one can cost.
- `seq` ran straight through the outage on every node, with no gap and no repeat.
  Delivery resumed on replug without anything re-sent to a node. The "one more stored"
  in the table is a node log line dropped on the USB link, not a record.
- Once the bridge is powered, it acks whether or not a host is reading it. Batches
  acked after `wartui` stopped were counted as reported and never stored, so a host
  that stops while the fleet keeps running loses those sightings, as it did with
  broadcast.
- As the Bluetooth node, `10:D0` lost 24 of 202 heartbeats, in single beats about every
  5.5 s. On the drive it lost 0.4% in the same role. The lost beats do not follow its
  own batches (a batch in 16% of the cycles that lost a beat, against 18% of those
  that did not), nor do they coincide with other nodes' frames. Heartbeats still
  broadcast, so this is outside the change.

## The change on the road

Measured on 2026-09-27: a 77-minute drive over much of the first drive's route, but not
all of it, with the same six nodes (`10:D0` on Bluetooth) and the C6 bridge `9D:24`.
40,466 frames, about 69 km.

| node | batches | lost | same `seq` twice (identical payload) | heartbeat loss | median link RSSI |
| --- | --- | --- | --- | --- | --- |
| 10D0 (BLE) | 2672 | 0 | 68 (54) | 0.3% | -56 |
| 4F98 | 5928 | 0 | 128 (102) | 4.3% | -58 |
| 5784 | 3451 | 0 | 266 (135) | 13.4% | -63 |
| 5950 | 1504 | 0 | 22 (21) | 0.2% | -54 |
| 4F08 | 2233 | 0 | 38 (33) | 0.7% | -58 |
| C5B8 | 2089 | 0 | 22 (17) | 1.2% | -57 |

- No batch was lost after the bridge's radio took it, against 250 (3.0%) on the first
  drive. The air was no kinder: heartbeats, which still broadcast, lost 3.3% against
  2.7%.
- 544 batches arrived twice under the same `seq`:
  - 362 were byte-identical and about 4 ms apart: the radio retransmitting after a
    lost ack. ESP-NOW on the bridge passes both copies up, so the host stored the
    same records twice: 1,152 observation rows repeat one stored within 50 ms,
    2.9% of the drive. 361 of the 362 arrived within 100 ms of the original; one came
    seconds later, which is a node re-sending the same addresses at the same RSSI
    after an unacknowledged send, not a retry. The engine now drops a batch that
    repeats its node's last `seq` and bytes within 150 ms by the bridge's own
    clock, which a node's next report cannot beat.
  - 182 reused the number with different records: every retry of a batch that did
    arrive went unacknowledged, so the node kept its `seq` and sent those addresses
    again later. Those are recorded.
- `5784` carried most of both kinds. Its link sat about 10 dB below the first drive's
  (median -63 against -51 dBm) and it lost 13% of its heartbeats, yet every one of its
  batches arrived. Where it sits in the car is worth a look.
- The Bluetooth node lost 0.3% of its heartbeats, so the bench's 12% did not recur.
- Unique Wi-Fi networks per km rose from 110 to 189, but the routes differ. On the 119
  cells of about 220 × 240 m that both drives covered, this drive found 14% more unique
  networks (4,369 against 3,838). That is consistent with the change, but time of day,
  speed and route are not held still, so it is not a measure of it.

## Open questions

- Swapping in a different bridge mid-run is covered by an engine test and not yet
  run on hardware.
- How many batches the radio's retries recovered on the road. An unacknowledged batch
  is invisible to the host, so the node's `unacked` count, in its own log, is the only
  record of it, and no node log was captured on this drive.
