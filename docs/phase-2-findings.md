# Phase 2 — what channel masks and Bluetooth-by-assignment actually do

Measured on 2026-09-07. The bridge throughout is an ESP32-C6 running
`firmware/bridge` (`9D:24`, USB). The node is one ESP32-C5 (`57:84`) on
`--features esp32c5,ble` unless a section says otherwise; the second C5
(`4F:98`) is flashed with the same build but contributed nothing, for the reason
in "Still not measured". Pool `us`, plaintext, control channel 6.

Access point addresses and names are left out deliberately, as in Phase 1:
these were real captures of a real neighbourhood, and a BSSID is exactly what a
geolocation database is built from. Counts and channels carry none of that and
are the whole of what was being checked.

Four captures, all on the same board and the same room within half an hour:

| | build | duration | what it was for |
| --- | --- | --- | --- |
| A | `esp32c5,ble` | 155 s | one assignment, both runs, no rotation |
| B | `esp32c5,ble` | 270 s | Bluetooth given and taken away again |
| C | `esp32c5` | 155 s | the control for the Bluetooth controller |
| D | `esp32c5,ble` | 240 s | B again, after a reflash, on the auto planner |

## One assignment covers both runs of the pool, and nothing rotates

`A` on a lone node produced exactly one `MSG_ADMIN`, epoch 1, carrying all
thirty-four channels of the US pool — `34: 1-11,36-165` in the fleet table —
acknowledged in 6488 µs. It was still the only assignment row 155 seconds later,
and the header names no phase because there is no longer a phase to name.

This is the whole point of the fourteen-byte frame. The US pool is two runs with
a hole at channels 12, 13 and 14, and the vendor's contiguous `start`/`end` pair
could say only one of them at a time; `Plan::phase_count`, `MAX_PHASES`,
`rotation_dwell` and `phase_since` existed to alternate between them on a sixty
second timer. A forty-bit mask says both at once, so a lone node now holds the
entire pool continuously rather than half of it at a time.

## Every observation lands inside the mask

Across runs A, B and D, every Wi-Fi observation named a channel inside the mask
that node had acknowledged — 70, 86 and 68 of them, on 13 to 14 distinct
channels, with **none outside**. The mask is non-contiguous, so this is the
membership check of Phase 2's item 2 in its two-run form.

Around twenty assigned channels were silent in each run. Those are the DFS
channels above 100 and the empty 2.4 GHz ones; Phase 1 established that a C5
reaches DFS 52–64 and cannot reach 100 and above, so a silent channel there is
the regulatory refusal already documented, not a sweep that skipped it.

## The first channel of an assignment is no longer stepped over

The node logs a line per channel it had something new to report, and its dedup
ring is empty at boot, so the first sweep after an assignment is the one sweep
that is fully visible on its console. Run A's, in order:

```
ch 1: 4 new, 4 total, 0 dropped      <- adoption, then the first dwell
ch 2: 2 new, 6 total, 0 dropped
ch 5 / 6 / 7 / 10 / 36 / 48 / 56 / 149 / 161
ch 1: 1 new, 45 total, 0 dropped     <- the second sweep begins
```

Channel 1 is index 0, the lowest index of the assignment and precisely the one
the old cursor stepped past: adoption set the cursor *to* the first index and
the foot of the loop advanced off it before it was ever dwelt on. It is now the
first channel of the first sweep. The store agrees from the other end — the
lowest assigned channel was first heard 1.1 s after the acknowledgement, well
inside a 4.7 s sweep.

The bug was not new to the channel-set work; the bounds-based cursor had the
same shape. It is fixed in `plan::SweepCursor`, on the host side of the split,
where four tests hold it.

## Bluetooth by assignment, and the node still acknowledges

This is the failure the whole project is downstream of, inverted. Phase 0's
vendor node with BLE on acknowledged **0 of 32** admin frames: NimBLE held the
one 2.4 GHz antenna through exactly the 300 ms window the node was meant to be
listening in, so the fleet split silently did not happen.

Run B gave one node the scan and took it away again, on the same channels
throughout, so all three frames are re-issues under a fresh epoch:

| epoch | flag | outcome | heartbeat to transmit callback |
| --- | --- | --- | --- |
| 1 | ble off | acked | 5876 µs |
| 2 | **ble on** | acked | 6980 µs |
| 3 | ble off | acked | 5832 µs |

The third row is the result. That frame was acknowledged in 5832 µs *while the
node was actively scanning Bluetooth* — the state in which a vendor node
acknowledges nothing at all. Run D reproduces it after a reflash: 5845 µs to
turn the scan on, 5874 µs to turn it off.

The node's console confirms adoption rather than merely acknowledgement:

```
assigned v1: 34 channels (1,2,...,165), ble off, node 0 of 1
assigned v2: 34 channels (1,2,...,165), ble on,  node 0 of 1
```

## Revoking the scan stops it, one sweep later

Run B recorded 89 Bluetooth observations and run D 85, and in both every one of
them falls inside the window between the enabling acknowledgement and the
revoking one. Zero before, zero after.

The edges are one sweep wide in each direction: the first BLE row arrived 5.2 s
after the enabling ack and the last 5.2 s before the revoking one, against a
5.2 s sweep. That is the design rather than a delay — the flag rides in the
assignment frame, and the scan runs once per completed sweep, so a node cannot
start or stop scanning in the middle of one.

## The scan costs about a tenth of a sweep, not a doubling

Sweep period is the heartbeat interval, since a node heartbeats once per
completed sweep. Same board, same thirty-four channels, three consecutive
stretches of one capture:

| run B stretch | sweeps | median |
| --- | --- | --- |
| ble off (epoch 1) | 17 | 4.709 s |
| **ble on (epoch 2)** | 20 | **5.221 s** |
| ble off (epoch 3) | 14 | 4.707 s |

Run D, after a reflash: 4.712 s, **5.222 s**, 4.705 s over 8, 23 and 17 sweeps.
Two independent captures agree on the loaded figure to a millisecond.

That is **+10.9%**, and it returns to baseline the moment the scan is revoked.
Phase 0 measured roughly a doubling on the vendor firmware. Checkpoint item 5
asked that the ~2× penalty not reappear, and it does not.

The arithmetic is unsurprising: thirty-four dwells at 125 ms is 4.250 s, the
admin window adds 300 ms once per sweep, and the remainder is the hop back to
the control channel and the report after each dwell. `BLE_INTERVAL_MS` is
`NUM_SCAN_CHANNELS * CHANNEL_DWELL_MS` — 5 s — so against a 5.2 s sweep the scan
runs on essentially every one: 21 scans over 20 sweeps in run B's loaded
stretch, each hearing about 50 advertisers.

## An initialised-but-disabled controller costs nothing measurable

`docs/phase-1-findings.md` closes on this, and it is the state Phase 2 makes
ordinary: a `ble` build now spends its whole life with `BleConnector::new` done
and `HCI_LE_Set_Scan_Enable` never sent, because the scan is a per-node
assignment that defaults to off. Phase 0 blamed the vendor node's lost
assignments on an initialised NimBLE stack holding the radio, which is
uncomfortably the same shape.

Run C is the control: the same board, the same assignment, the same room, on a
build with the `ble` feature left out entirely.

| | median sweep |
| --- | --- |
| A — `ble` in, scan never enabled | 4.764 s |
| B stretch 1 — same build, same state | 4.709 s |
| B stretch 3 — same build, same state | 4.707 s |
| D stretch 1 — same build, after a reflash | 4.712 s |
| **C — `ble` left out** | **4.696 s** |

The plain build is 11 ms faster than the closest `ble` build and 68 ms faster
than the furthest. But the four `ble` rows are *the same binary on the same
board*, and they span 57 ms between themselves. A build difference that is
smaller than the spread of identical firmware is not a measurement of the build.

So: an initialised controller with no scan enabled costs nothing this bench can
see, and the Phase 1 suggestion — bring the controller up on the first
assignment rather than at boot — is not warranted. Bringing it up at boot keeps
the surprise out of the moment an assignment arrives, which is what
`firmware/node/src/main.rs` says it is for.

**What is unexplained is run A's 55 ms.** It is the same binary as B and D, and
it was steady within itself (4.761–4.788 across 28 sweeps). The second node was
on the air for the first third of run A and went silent partway through, but
that is not the cause: restricted to the sweeps after it went quiet, run A still
measures 4.764 s. Whatever moves it is not the Bluetooth feature, since it moves
between two runs that share it.

## Still not measured

- **Two nodes.** Every result above is one node. The dealt partition — the
  planner flattening the pool and giving index `k` to node `k mod node_count`,
  so each node gets a comb of both bands — has never been on hardware, and
  neither has "no *other* node emits Bluetooth". A comb is also the only shape
  that exercises the scattered rendering in the fleet table and the mask
  arithmetic at its widest.
  `4F:98` was reflashed with the current build but came up
  `boot:0x8 (DOWNLOAD(UART0/USB))` and sat in the ROM waiting for a download,
  so it never ran the application. That is a strap held low, which no amount of
  `espflash reset` will clear.
- **The stagger, again.** Unchanged from Phase 1: it cannot be disabled from the
  host, so "it worked" and "there was nothing to prevent" remain inseparable
  without a firmware build that omits it.
- **A dense room.** The Bluetooth penalty was measured where about fifty
  advertisers answered each scan. Whether it stays near a tenth of a sweep where
  five hundred do is unknown, and this bench cannot produce that.
- **Whether channel 14 is genuinely unreachable on hardware.** Phase 2 removed
  it from every pool on the strength of the Phase 1 measurement and the
  `esp-radio` country blob, and no run since has tried to tune it. It is now
  unreachable by construction from the host, which is the intended state but
  also means the original refusal is no longer being re-checked.
