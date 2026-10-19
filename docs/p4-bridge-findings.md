# The P4 split bridge: measured against the single-chip bridge

These measurements back the P4 split bridge. The design is in
[`docs/superpowers/specs/2026-10-17-p4-split-bridge-design.md`](superpowers/specs/2026-10-17-p4-split-bridge-design.md),
and the SPI link spike is in [`p4-spi-link-findings.md`](p4-spi-link-findings.md).

Every run uses the same load:
- **Nodes:** two, both running the burst-test node build from local branch
  `bench/burst-load-1.2` (f67ec33). They are `F5:50` (ESP32-C5) and `9A:24` (XIAO ESP32-C6
  with an external antenna).
- **Host:** `wartui run` for 10 minutes with `--no-gps` and a fixed test position, then
  `wartui analyze`.
- **Bench:** the P4 board stays unplugged during the single-chip runs, because its spike C6
  announced itself as core.

## Baseline: single-chip bridge, blocking send (2026-10-17)

The bridge is `75:40` (XIAO ESP32-C6 with an external antenna), running `firmware/bridge` at
f71b902, which is `main` plus documents only.

| | |
|---|---|
| span | 10 min (03:13–03:23 UTC) |
| frames the bridge received | 11,373 |
| frames its USB outbox dropped | **1,425 (12.5%)** |
| frames the host read | 9,962 |
| batches lost, node to host | 1,425: `F5:50` 647, `9A:24` 778 |
| heartbeats missed | 0 of 272 |
| host behind ≥100 ms | once, worst 14.4 s, at attach while the backlog drained |
| assignments | 2, both acked; latency 1.0 ms and 3.9 ms |

**Every lost batch is an outbox drop.** The 1,425 batches lost between node and host equal the
1,425 frames the outbox discarded. None were lost on the air, and none in the radio's
receive queue. Under this burst load, the single-chip bridge's USB path is what limits the
fleet. That is the headroom the P4 bridge is meant to add.

## Sharing the radio code cost the single-chip bridge about 1% (2026-10-18)

Moving the bridge's radio half into `firmware/common` (f3c1ca4) made the same bridge, under the
same load, drop more. Each row is one 10-minute run on `75:40`:

| Build | Frames received | Outbox drops | Batches lost |
|---|---|---|---|
| f71b902, baseline | 11,373 / 11,380 | 1,425 / 1,425 | 1,425 / 1,446 |
| f71b902, with the P4 attached and running | — | — | 1,419 |
| f3c1ca4, radio half shared | 11,374 | 1,755 | 1,781 |
| f3c1ca4, yielding only while a send waits | 11,461 | 1,586 | — |
| 58b43ce, both causes fixed | 11,385 | **1,551** | 1,569 |

The load is deterministic: the baseline dropped exactly 1,425 frames in both runs. The host
built from this branch cannot read the old firmware's `Status`, so the control run gives
batches lost only. Batches lost tracks outbox drops one for one.

Two causes were found and fixed in 58b43ce:
- **A yield on every busy pass.** It let the joined send future run during a burst, at the cost
  of drain time. The loop now yields only while a send waits.
- **A fixed 1,024-byte copy per forwarded frame.** `radio::hear` returned the frame by value,
  and at opt-level `s` that copied the whole payload buffer a second time. It now hands the
  frame to a closure, and `forward` is back to the baseline's one payload-length copy
  (checked in the disassembly).

The remaining gap is about 1% fewer frames delivered under this burst. The P4 being attached
does not explain it. The likeliest remaining cost is per wake: during a burst the loop wakes
each time the USB FIFO empties, and each wake now also polls the send future. It was not
pursued: the user accepted about 1% as the price of one radio half shared between the
single-chip and split bridges. Wanting more is what the P4 bridge is for.

## The P4 split bridge on the ESP32-P4-WIFI6-POE-ETH (2026-10-18)

The P4 is `8B:65` and its C6 is `D8:1C`, both running this branch. The nodes and the load are
the ones above, and the single-chip figures are `75:40`'s.

### Under the burst load, it lost nothing

| | P4 bridge, 58b43ce | single-chip, f71b902 |
|---|---|---|
| frames received | 11,305 | 11,373 |
| frames dropped | **0** | 1,425 (12.5%) |
| batches lost, node to host | **0** | 1,425 |
| host behind, after attach | under 1 ms | about 28 ms |
| assignment latency | 5.0 and 6.4 ms | 1.0 and 3.9 ms |
| SPI errors | 0 in 1,015,634 transfers | — |
| heartbeats missed | 43 of 251 (17.1%) | 0 of 272 |

Every P4 run lost no batch: seven burst runs, the C6 restart, and the production session. That
holds through every antenna below. Assignment latency stayed between 4.0 and 6.4 ms across the
runs, about 3 ms more than the single-chip bridge, which is the SPI hop each way. That is well
inside the node's 100 ms admin window. `ready_timeouts` reads 2 in every life, both at boot,
before the C6 first arms.

### Heartbeats are lost on the air, on this board

The P4 bridge misses heartbeats that the single-chip bridge receives. Heartbeats broadcast,
so the radio never retries one. Batches unicast and are retried until acknowledged, which hides
the same loss. No batch was lost, so nothing was dropped after the C6 acknowledged it: the loss
happens before reception.

| Run | `F5:50` | `9A:24` | Overall |
|---|---|---|---|
| shipped flat antenna | 16% | 42% (−57 dBm) | 28.9% |
| flat antenna, after a replug | 6.4% | 22.5% | 14.3% |
| larger antenna | 2.4% (−31 dBm) | 88% (−67 dBm) | 24.1% |
| larger antenna, moved | 5.6% (−39 dBm) | 17.5% (−53 dBm) | 11.5% |
| flat antenna, turned | 4.7% (−42 dBm) | 20.5% (−62 dBm) | 12.4% |
| larger antenna, turned | 11.0% (−33 dBm) | 15.3% (−56 dBm) | 13.1% |
| `75:40`, external antenna | 0% (−17 dBm) | 0.8% (−39 dBm) | 0.4% |

- **It is not the SPI link or the load.** Heartbeats arrive every 5.0 s, so each missing one
  can be placed in time. In every run, missed heartbeats fall inside a burst no more often
  than received ones, although a burst runs the SPI link and USB flat out. A run in which the
  P4 rested 5 ms after each transfer idle both ways (about 1,600 idle transfers/s down to 177)
  lost about half as many, but its nodes also arrived 5 dB stronger. Every later run kept that
  rest, and none came close to the single-chip bridge.
- **It is not the radio setup.** The C6 runs the same `esp-radio` features, the same
  controller configuration and the same `radio::setup` as the single-chip bridge.
- **It is not signal strength alone.** At −39 dBm `75:40` loses 0.8% and this C6 loses 5.6%.
  `F5:50` lost more at −33 dBm than at −39. The likeliest cause is noise from the board reaching
  the C6, or loss in its RF path. The antenna's cable is short, and every route passes the PoE
  circuitry, with the Ethernet cable unplugged. The WIFI6 board, which has no PoE stage, will
  show whether this is the board.

The user accepted the loss for now, since no sighting is lost. It needs a better antenna
placement, or another board.

The 5 ms rest is not in the firmware. It raised assignment latency to 20–42 ms, because each
step of a send waits out a rest. A link that waits for work has to be woken by work, which
is a follow-up.

### Restarting the C6 alone reads as a new bridge life

A test build pulsed the C6's EN low for 50 ms, 180 s into the P4's life. The host logged a
fresh `Ready` with `uptime_ms=72` at once, and `wartui analyze` counted one reboot. Both nodes
were heard again within 3 and 6 seconds and went on reporting. No frame was dropped and no
batch was lost.

After the review fixes, which tell a restart by the C6's transfer counter going backwards,
the check was repeated (2026-10-18). The host logged a fresh `Ready` with `uptime_ms=71` and
one reboot, and an assignment sent after the restart was acknowledged in 5.1 ms. No frame was
dropped and no batch was lost. The SPI counters recorded one CRC failure, one sequence gap and
5 READY timeouts, against none, none and 2 in runs without a restart. That fits resetting the
C6 in the middle of a transfer.

### A production session behaves as on a single-chip bridge

Over 10 minutes with production node firmware, both nodes joined and both assignments were
acknowledged, in 4.6 and 4.4 ms. Both nodes ran at the 2 dBm default transmit power. No frame
was dropped and no batch was lost, over 9,117 sightings.

At attach, the host read 243 frames the P4 had queued while nothing was reading. That
included 31 old heartbeats, each counted as a missed admin window. No live window was missed.
The single-chip bridge does the same thing, but its outbox is a few kilobytes, while the P4's
bulk queue is 256 KB. That can be over a minute of old frames: 78 s at worst in these runs.

### The first host query after power-up sometimes goes unanswered

Twice, a `wartui status` sent about 10 seconds after the P4 powered up got no answer within
5 s, and the next one, seconds later, answered at once. Once more, the first query answered at
once. The cause is not known yet. A second query has always answered.

### Queue high-water marks suggest smaller C6 queues

After the final burst run, read with `wartui status`:

| Queue | Most held | Size | Dropped |
|---|---|---|---|
| C6 radio up | 3,118 B | 98,304 B | 0 |
| C6 send payloads | 1 | 4 | 0 |
| C6 radio jobs | 1 | 32 | 0 |
| C6 priority up | 36 B | 8,192 B | 0 |
| P4 radio down | 28 B | 16,384 B | 0 |
| P4 host priority | 62 B | 16,384 B | 0 |
| P4 host bulk | not measured under load | 262,144 B | 0 while a host read |

- **The C6's up ring peaked at 3%** under a two-node burst, because the SPI link drains it
  faster than the radio fills it. 16 KB would leave a five-fold margin. A 20-node fleet should
  be measured before it shrinks that far.
- **The host-bulk mark could not be read during a run.** `wartui run` holds the port, and the
  captures' `bridge_status` rows do not store `Status.queues`. Each status read after a run
  found the queue filled by frames that arrived once the host stopped reading. Recording the
  queue stats in captures would close this gap.
- **The radio-jobs and priority-up rows come from the restart check above,** not the burst
  run: those queues were added by the review fixes. The priority ring carries only the C6's
  own records, such as `Ready` and acknowledgements, so its load does not grow with sightings.
- The other defaults stand. They held at least ten times what they were asked to.
