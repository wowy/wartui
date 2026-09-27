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

## The change on the bench

Measured on 2026-09-27 with the 5 s heartbeat build (`ASSIGNED_BEAT_MS`) on a XIAO C6 bridge
`00:08` with an external antenna and two C5 nodes, `0A:28` and `7E:3C`, all on one desk. The
capture ran 20 minutes with `--record-raw`, with both nodes' serial logs read alongside. In turn
it covered: Wi-Fi only; `0A:28` given the Bluetooth scan and then relieved of it; `7E:3C` held
in its ROM bootloader (`espflash board-info --after no-reset`) past the 60 s timeout, then reset.

| phase | node | share | heartbeat gap, mean (range) | sweep period, from counters | `beat` shown |
| --- | --- | --- | --- | --- | --- |
| Wi-Fi only, 10 min | 0A:28 | 21 ch | 4999 ms (4881–5028) | 2754 ms | 2.8 s |
| | 7E:3C | 20 ch | 5000 ms (4885–5046) | 2642 ms | 2.5 s |
| Bluetooth on 0A:28, 5 min | 0A:28 | scan | 4997 ms (4707–5254) | 524 ms | 525 ms |
| | 7E:3C | 41 ch | 5083 ms, one beat lost | 5351 ms | 5.0 s |
| 7E:3C offline | 0A:28 | 41 ch | 4897–5026 ms | 5496 ms | 5.0 s |

- Heartbeats kept their interval with no drift. Each gap stayed within one dwell (sweeping) or
  one scan (Bluetooth) of 5 s, which is how often each checks the deadline. 3 of 483 heartbeats
  were lost.
- A sweep costs about 131 ms a channel with the window amortised in: a dwell, the hop, and
  100 ms of window every 5 s.
- The Bluetooth node scanned every ~514 ms, 613 scans in its log. Its batches come as often as
  it hears a new advertiser, so the median gap is 2.7 s, but the shortest are one scan apart.
- Each of the nine assignments was sent on the heartbeat that made it due, acknowledged first
  time, and delivered 1–3 ms after it was created. A keypress re-cut was adopted by both nodes
  2.4–4.2 s later. The offline node's share went to `0A:28` 63 s after it went quiet. Once reset,
  it was dealt its share 1.6 s after boot through its idle beat, and `0A:28` got its own at the
  next heartbeat, 3.4 s after that.
- Every record either node logged was stored. On `0A:28` that is 281 Wi-Fi and 184 Bluetooth, in
  both the log and the store. The node's `unacked` count stayed at 0 throughout.
- `beat` measures a sweep period longer than the heartbeat interval: 5.0 s shown for a 5.5 s
  sweep, against the ~5.0 s it would read if the gaps with no counter step were dropped. The
  span over five intervals is coarse, though: it is off by up to one sweep across ~25 s, so
  about 10% at 2.6 s sweeps and 10–20% at 5 s ones. #97 replaces the column with the node's own
  report of the epoch it holds.
