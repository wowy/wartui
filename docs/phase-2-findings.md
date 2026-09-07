# Phase 2 — what channel masks and Bluetooth-by-assignment actually do

Measured on 2026-09-07. The bridge throughout is an ESP32-C6 running
`firmware/bridge` (`9D:24`, USB). The nodes are two ESP32-C5s (`57:84` and
`4F:98`) on `--features esp32c5,ble` unless a section says otherwise. Runs A to
D are one node; run E is both. Pool `us`, plaintext, control channel 6.

Access point addresses and names are left out deliberately, as in Phase 1:
these were real captures of a real neighbourhood, and a BSSID is exactly what a
geolocation database is built from. Counts and channels carry none of that and
are the whole of what was being checked.

Six captures. A to D are one board within half an hour; E is both boards, nine
hours later; F adds a third board, `59:50`, which turned out not to be running
this firmware at all — see "A node that is not ours". Same room throughout.

| | build | duration | what it was for |
| --- | --- | --- | --- |
| A | `esp32c5,ble` | 155 s | one assignment, both runs, no rotation |
| B | `esp32c5,ble` | 270 s | Bluetooth given and taken away again |
| C | `esp32c5` | 155 s | the control for the Bluetooth controller |
| D | `esp32c5,ble` | 240 s | B again, after a reflash, on the auto planner |
| E | `esp32c5,ble` | 240 s | two nodes, the dealt partition, one Bluetooth scan |
| F | `esp32c5,ble` ×2 | 135 s | three nodes, one of which is not ours |

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

## Channel membership holds, for a node holding the pool

Across runs A, B and D — one node holding the whole pool — every Wi-Fi
observation named a channel inside the mask that node had acknowledged: 70, 86
and 68 of them, on 13 to 14 distinct channels, with **none outside**. The mask
is non-contiguous, so this is the membership check of Phase 2's item 2 in its
two-run form. Run E, where two nodes hold interleaved combs, is where this stops
being the whole story — see "Adjacent channels bleed, and the deal is what
exposes it".

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

## Two nodes are dealt a partition, and it is exact

Run E, on the auto planner. The second node joined a fleet where the first held
the whole pool, and the re-cut landed on both:

| node | index | channels |
| --- | --- | --- |
| `4F:98` | 0 of 2 | `1,3,5,7,9,11,40,48,56,64,112,120,128,136,144,153,161` |
| `57:84` | 1 of 2 | `2,4,6,8,10,36,44,52,60,100,116,124,132,140,149,157,165` |

Seventeen each, **no overlap**, and the union is exactly the thirty-four
channels of the US pool. Shares differ by zero. That is the round-robin deal
working as designed: index `k` of the flattened pool to node `k mod 2`, so each
node carries part of 2.4 GHz *and* part of 5 GHz rather than one taking each
band. It is also a shape the vendor's contiguous `start`/`end` pair could not
have expressed at all — not as one frame, and not as any number of them.

Sweep periods track the halved shares: 2.51 s and 2.56 s against seventeen
dwells at 125 ms plus the 300 ms admin window, or 2.425 s of arithmetic.

## Adjacent channels bleed, and the deal is what exposes it

Run E is the first capture where a node reported a channel it had not been
assigned. `57:84`, holding the even 2.4 GHz channels, reported 16 observations
on 1, 3, 5, 9 and 11; `4F:98`, holding the odd ones, reported 7 on 6 and 8.

**Every one of them is exactly one channel away from a channel that node did
hold, and every one is 2.4 GHz.** Not a single 5 GHz observation landed outside
a mask in any run.

That is adjacent-channel capture, and it is correct behaviour rather than a
sweep escaping its assignment. 2.4 GHz channels are 5 MHz apart and 20 MHz
wide, so a radio parked on channel 2 hears beacons transmitted on 1 and 3; the
node then reads the transmitting channel out of the beacon's own DS Parameter
Set (tag 3) and reports *that*, which is the truth about the access point. The
alternative — labelling it with the parked channel — would be the bug Phase 1's
narrow-assignment check exists to catch. 5 GHz channels in this pool are 20 MHz
apart and do not overlap, which is why the effect is one-sided.

Phase 1 saw none of this because its two-node split was contiguous: `4F:98` took
all of 2.4 GHz and `57:84` all of 5 GHz, so there was no 2.4 GHz neighbour in
anyone else's share to bleed from. The round-robin deal interleaves adjacent
2.4 GHz channels *between* nodes, which is the arrangement that maximises it.

The cost is duplication, not error: **23 of 101 distinct access points were
reported by both nodes**, and all 23 sit on 2.4 GHz channels. The dedup ring is
per-node, so neither node repeats itself and neither knows what the other heard.
`export` already picks one row per network, so a WiGLE file is unaffected; what
grows is the store, by about a fifth of the 2.4 GHz rows in a two-node fleet.

This is a consequence of the deal that the plan did not anticipate. It does not
change the argument for round-robin — every node still gets both bands, which
is what block-splitting cannot give — but "channel membership" now needs saying
carefully: a node *dwells* only where it was assigned, and the channel on an
observation is the access point's, not the dwell's.

**Nothing is being changed for it.** Dropping a sighting whose DS Parameter Set
names a channel outside the assignment would throw away a real access point,
found on a frequency the node was legitimately listening to, and would make the
node's report agree with the plan by deleting evidence. The duplication it
avoids is a fifth of the 2.4 GHz rows in a two-node fleet, which the export
already collapses.

## Only the assigned node scans Bluetooth

Run E gave the scan to `4F:98` while `57:84` ran the identical binary, `ble`
feature and all. `4F:98` produced 81 Bluetooth observations; `57:84` produced
**none at all**, at any point in the capture. Zero arrived before the enabling
acknowledgement or after the revoking one, on either node.

Both frames were acknowledged — 5852 µs to turn the scan on and 5831 µs to turn
it off, the latter sent while the node was scanning. That is the same result as
runs B and D on a second board.

The fleet table at t+140 s, with the scan held:

```
node              rssi beats obs  last beat  ble  channels            state
38:44:BE:1F:4F:98 -39  61    138  1s   3.0s  ble  17: 1,3,5,7,9,11…   alive
38:44:BE:1F:57:84 -57  64    56   0s   2.6s       17: 2,4,6,8,10,36…  alive
```

Which is also the first hardware sighting of the scattered channel cell: the
count leads, the list is cut at a comma rather than mid-range, and there is one
ellipsis rather than two. No frame in any of the five captures rendered `……`.

## Three nodes, and the shares differ by one

Run F put three nodes in front of the planner. The deal:

| node | index | channels |
| --- | --- | --- |
| `4F:98` | 0 of 3 | 12: `1,4,7,10,40,52,64,116,128,140,153,165` |
| `57:84` | 1 of 3 | 11: `2,5,8,11,44,56,100,120,132,144,157` |
| `59:50` | 2 of 3 | 11: `3,6,9,36,48,60,112,124,136,149,161` |

Twelve, eleven and eleven. Pairwise overlap is zero in all three pairs and the
union is exactly the thirty-four channels of the US pool, so `shares differ by
at most one` — which had only ever been a host test — holds on hardware at the
first node count where it does any work. Thirty-four does not divide by three,
and the remainder lands on the lowest index.

Sweep periods follow the shares closely enough to predict from the count. Taking
129.7 ms per channel from the earlier runs and adding the 300 ms admin window:

| node | channels | predicted | measured |
| --- | --- | --- | --- |
| `4F:98` | 12 | 1.86 s | 1.86 s |
| `57:84` | 11 | 1.73 s | 1.77 s |

## A node that is not ours acknowledges, and does not adopt

The third board never ran this firmware. It could not be flashed — its port
gives the `Secure Download Mode is enabled on this chip` that Phase 1 identified
as espflash inferring the mode from silence — and it was already on the air
behaving like a stock node: heartbeating every 6.47 s and reporting access
points *with no assignment at all*, where an unassigned wartui node parks on the
control channel at 1 s and collects nothing. All forty channels by default is
`WiFiOps.cpp:77-80`.

So run F is an accidental but exact test of an invariant that had never been on
hardware. Core to node is wartui's own fourteen-byte frame; the vendor's ten-byte
one does not decode, and nothing is done to keep the two working together.

The host sent `59:50` eleven channels and the frame was **acknowledged in
5920 µs** — its radio acknowledges at the MAC layer whatever the application
then does with the bytes. It did nothing with them:

| | told to hold | that predicts | measured |
| --- | --- | --- | --- |
| `4F:98` | 12 channels | 1.86 s | 1.86 s |
| `57:84` | 11 channels | 1.73 s | 1.77 s |
| **`59:50`** | 11 channels | 1.73 s | **6.25 s** |

6.25 s against 6.47 s before the assignment: unchanged, and three and a half
times what its share predicts. It also reported channel 5, which is in nobody's
share but its own reading of the whole band. The host, meanwhile, recorded the
assignment as `acked` and showed the node holding eleven channels.

**The cost is not that the foreign node is useless. It is that it takes a third
of the pool with it.** Eleven channels were cut out of the plan for a node that
was never going to scan them, so those channels went uncovered by the fleet
while the fleet table said otherwise. Two nodes and a stranger cover less of the
pool than two nodes alone.

This is the hazard the Phase 2 plan's capability token was for — a short ASCII
token in the heartbeat's text field, `wartui/0.1;ble,5g`, so the host learns
what a node is from its first heartbeat and before it has to choose anything.
It was specified in the plan and not built, and nothing else can distinguish the
two firmwares: node to core is byte-identical by design, which is exactly what
lets vendor golden vectors keep testing this code.

## The bleed does not obviously grow with node count

At three nodes each 2.4 GHz channel has both of its neighbours in other nodes'
shares, where at two it had one. The rate did not clearly move:

| | 2 nodes (run E) | 3 nodes (run F) |
| --- | --- | --- |
| `4F:98` | 7/61 — 11% | 8/45 — 18% |
| `57:84` | 16/63 — 25% | 5/60 — 8% |

Every observation outside a mask was again exactly one channel from one the node
held, and again every one was 2.4 GHz. The two runs bracket each other rather
than separating, and forty to sixty observations per node is too few to call a
difference of this size, so the honest reading is that both are the same
order and this bench cannot resolve better than that.

## Still not measured

- **More than three nodes**, and any count where two nodes take the remainder
  rather than one. Three is the first count at which the shares are uneven and
  it works; four, where thirty-four splits 9/9/8/8, is untested.
- **A three-node fleet that is actually three of ours.** The deal, the sweep
  periods and the bleed above are sound — the planner's arithmetic does not care
  what adopts it — but only two of the three nodes were running this firmware,
  so "no *other* node scans Bluetooth" is still checked at two nodes rather than
  three, and the third share was never actually swept.
- **The stagger, again.** Unchanged from Phase 1: it cannot be disabled from the
  host, so "it worked" and "there was nothing to prevent" remain inseparable
  without a firmware build that omits it.
- **What to do about a foreign node.** The capability token specified in the
  Phase 2 plan is not built, so wartui cannot tell a stock node from one of its
  own and cuts it a share regardless. What the host should then *do* — refuse to
  plan for it, plan around it, or only say so in the fleet table — is a decision
  nobody has taken.
- **A dense room.** The Bluetooth penalty was measured where about fifty
  advertisers answered each scan. Whether it stays near a tenth of a sweep where
  five hundred do is unknown, and this bench cannot produce that.
- **Whether channel 14 is genuinely unreachable on hardware.** Phase 2 removed
  it from every pool on the strength of the Phase 1 measurement and the
  `esp-radio` country blob, and no run since has tried to tune it. It is now
  unreachable by construction from the host, which is the intended state but
  also means the original refusal is no longer being re-checked.
