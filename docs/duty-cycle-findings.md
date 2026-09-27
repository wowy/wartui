# Duty cycle — where a node's time goes

Measured on 2026-09-27 from the unicast drive in `docs/batch-loss-findings.md` § "The
change on the road": 77 minutes, six C5 nodes, `10:D0` on Bluetooth, the C6 bridge `9D:24`,
every frame kept with `--record-raw`. The question was how much of each node's cycle goes
to sniffing, to sending what it heard, to its heartbeat and to the admin window, and what
that becomes as a fleet grows past ten nodes.

The plan settled 11 s in and nothing re-cut it afterwards. From then on, four nodes held
8 channels, `4F:98` held 9 (41 in all, the `all` pool less channel 14), and `10:D0` held
Bluetooth.

## Method

- A node's cycle is its heartbeat period, Δ`rx_at` / Δcounter between consecutive heartbeats.
  Dividing by the counter step keeps a lost heartbeat from doubling a gap.
- Each cycle was binned by how many sighting batches arrived inside it. The period with no
  batches is the fixed cost: dwells, hops, stagger and window. The slope over the bins is
  what one batch costs.
- What a no-batch cycle has left, after the dwells (`n × CHANNEL_DWELL_MS`), the stagger
  (`stagger_offset_ms`) and `ADMIN_WAIT_MS`, is the cost of leaving and returning to the
  control channel once per channel.

All of it comes from `raw_frame` alone: the type byte at offset 5, the heartbeat counter as
a little-endian `u32` at 6, and `rx_at` in host milliseconds.

## Findings

| node | ch | stagger | cycle, median | sniffing | hops | reports | stagger wait | heartbeat + window |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C5B8 | 8 | 100 ms | 1238 ms | 1000 (81%) | 37 (3%) | ~2 | 100 (8%) | ~101 (8%) |
| 4F08 | 8 | 80 | 1218 | 1000 (82%) | 37 (3%) | ~2 | 80 (7%) | ~101 (8%) |
| 5950 | 8 | 60 | 1195 | 1000 (84%) | 34 (3%) | ~1.5 | 60 (5%) | ~101 (8%) |
| 5784 | 8 | 40 | 1177 | 1000 (85%) | 34 (3%) | ~3.5 | 40 (3%) | ~101 (9%) |
| 4F98 | 9 | 20 | 1290 | 1125 (87%) | 40 (3%) | ~6 (0.5%) | 20 (2%) | ~101 (8%) |
| 10D0 (BLE) | — | 0 | 1000 | 500 scan (50%) | — | ~2 | 0 | ~498 (50%) |

- A sweeping node sniffed for 81–87% of its cycle. Nearly all of the rest was fixed waiting:
  the admin window took about 8%, and the stagger took 2–8% by node index.
- A batch costs 3.5–4 ms, the same on every node. On `4F:98` the period went from 1285 ms
  with no batches to 1319 ms with six. Even `4F:98`, at 1.67 batches a cycle, spent
  under 1% reporting.
- The hop out and back costs about 4.5 ms a channel, or about 2.3 ms per `radio::park`:
  3% of the cycle.
- All 17 assignments went out in the first 11 s. The other ~3,700 windows on each node
  carried nothing.
- The stagger separated nothing. It is a delay rather than a phase, so it makes each node's
  period different (1177 to 1238 ms on the 8-channel nodes), and their relative phase drifts
  through the whole cycle every minute or so. 697 of 22,540 heartbeats landed within ±3 ms
  of another node's, against about 637 if they were placed at random.
- The Bluetooth node scanned for `ble::SCAN_MS`, 500 ms, and held the control channel for
  the rest of `BLE_BEAT_MS`, listening for an assignment that never came.

## What it means at a larger fleet

With these constants, a sweeping node's cycle is about `n × 129.6 + stagger + 101` ms, with
`n` its share of the 41 channels. The window and stagger are paid once per sweep, and a
sweep shortens as the fleet grows:

| fleet | channels per node | cycle | sniffing, average node | sniffing, last index |
| --- | --- | --- | --- | --- |
| 6 (this drive) | 8.2 | ~1225 ms | 84% | 81% |
| 11 | 4.1 | ~690 ms | 75% | 70% |
| 20 (`MAX_NODES`) | 2.2 | ~440 ms | 62% | 55% |

A fixed heartbeat interval, rather than one per sweep, makes the window's cost independent
of the share. At one beat every 5 s it is about 2% of the node's time at any fleet size, and
sniffing is bounded by the hop cost alone: about 96% of a dwell-plus-hop. The price is
latency: an assignment waits up to one interval for its window, where it now waits up to
one sweep.
