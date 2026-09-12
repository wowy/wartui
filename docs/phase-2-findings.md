# Phase 2 — what channel masks and Bluetooth-by-assignment actually do

Measured on 2026-09-07. The bridge throughout is an ESP32-C6 running
`firmware/bridge` (`9D:24`, USB). The nodes are two ESP32-C5s (`57:84` and
`4F:98`) on `--features esp32c5,ble` unless a section says otherwise. Runs A to
D are one node; run E is both. Pool `us`, plaintext, control channel 6.

`file:line` citations below point into
[wowy/ESP32DualBandWardriver](https://github.com/wowy/ESP32DualBandWardriver) on
`feat/node-interference-mitigation`, the vendor firmware.

Access point addresses and names are left out deliberately, as in Phase 1:
these were real captures of a real neighbourhood, and a BSSID is exactly what a
geolocation database is built from. Counts and channels carry none of that and
are the whole of what was being checked.

Eleven captures. A to D are one board within half an hour; E is both boards, nine
hours later; F adds a third board, `59:50`, which turned out not to be running
this firmware at all — see "A node that is not ours" — and G is the same three
after it was flashed by hand. Same room throughout.

Runs I to K are a second bench later the same day, and the first with two kinds
of board on it: three ESP32-C5s (`4F:08`, `C5:B8`, `4F:98`) and two ESP32-C6s
(`75:40`, `00:08`), every one on a build with `ble` compiled in, same bridge and
same room. Only `4F:98` is carried over — `57:84` and `59:50` were off the bench
by then — so nothing in runs A to H should be read as continuous with these.

| | build | duration | what it was for |
| --- | --- | --- | --- |
| A | `esp32c5,ble` | 155 s | one assignment, both runs, no rotation |
| B | `esp32c5,ble` | 270 s | Bluetooth given and taken away again |
| C | `esp32c5` | 155 s | the control for the Bluetooth controller |
| D | `esp32c5,ble` | 240 s | B again, after a reflash, on the auto planner |
| E | `esp32c5,ble` | 240 s | two nodes, the dealt partition, one Bluetooth scan |
| F | `esp32c5,ble` ×2 | 135 s | three nodes, one of which is not ours |
| G | `esp32c5,ble` ×3 | 250 s | three nodes, all three of them ours |
| H | `esp32c5,ble` + one older | 115 s | the capability token, against a genuinely mixed fleet |
| I | `esp32c5,ble` ×2, `esp32c6,ble` | 120 s | a mixed fleet, and the deal by band |
| J | five boards, both chips | 240 s | five nodes, then four |
| K | `esp32c6,ble` then `esp32c6` | 282 s | the Bluetooth flag withdrawn by reflash |

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
a mask in any run. (Run G later produced one stray at *two* channels out, which
the geometry also allows — see "What the bleed rate actually depends on".)

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
02:00:5E:10:4F:98 -39  61    138  1s   3.0s  ble  17: 1,3,5,7,9,11…   alive
02:00:5E:10:57:84 -57  64    56   0s   2.6s       17: 2,4,6,8,10,36…  alive
```

Which is also the first hardware sighting of the scattered channel cell: the
count leads, the list is cut at a comma rather than mid-range, and there is one
ellipsis rather than two. No frame in any of the six captures rendered `……`.

## Three nodes, and the shares differ by one

Runs F and G put three nodes in front of the planner and got the same deal both
times, which is what a planner with no clock should do — the cut depends on the
fleet and nothing else. Run G is the one where all three nodes were ours:

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

| node | channels | predicted | measured (F) | measured (G) |
| --- | --- | --- | --- | --- |
| `4F:98` | 12 | 1.86 s | 1.86 s | 1.86 s |
| `57:84` | 11 | 1.73 s | 1.77 s | 1.77 s |
| `59:50` | 11 | 1.73 s | — | 1.82 s |

Run G also settles the Bluetooth question at three nodes: `4F:98` produced 84
advertiser records and `57:84` and `59:50` produced **none at all**, on the same
binary with the same feature compiled in. Its two frames were acknowledged in
5837 µs and 5871 µs, the second while it was scanning.

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
Nothing else can distinguish the two firmwares: node to core is byte-identical
by design, which is exactly what lets vendor golden vectors keep testing this
code. **It is built now, and the section below has the run.**

> **The stated cause stopped being true on 2026-09-10.** The measurement stands:
> a stranger in the fleet cost more than it contributed, and two nodes and a
> stranger covered less than the two alone. What changed is *why* the two
> firmwares were hard to tell apart. Node to core is no longer byte-identical —
> every frame carries wartui's own magic — so a stranger's heartbeat does not
> decode here at all and never reaches the fleet table. The reasoning above is
> what drove that: this run is a vendor node being mis-planned by wartui, and
> the same argument read in the other direction is wartui mis-planning a vendor
> core's fleet, which is what these nodes were doing to anyone in range. The
> capability bits survive the change, for the part that was always about our own
> nodes: which band a radio reaches, and whether the Bluetooth code is on it.

## What the bleed rate actually depends on, and it is not node count

Run G has all three dedup rings empty at the start, so it is the first capture
where the bleed can be read cleanly. Counting only 2.4 GHz rows, which are the
only ones that ever bleed:

| node | its 2.4 GHz share | rows outside it |
| --- | --- | --- |
| `4F:98` | 1, 4, 7, 10 | 15/35 — **43%** |
| `57:84` | 2, 5, 8, 11 | 17/33 — **52%** |
| `59:50` | 3, 6, 9 | 1/28 — **4%** |

A spread of 4% to 52% inside one capture, on one fleet, in one room. Node count
does not explain that, and neither does anything about the firmware. **Where the
room's access points sit does.** The distinct access points this room offers,
by channel:

```
ch  1: 17     ch  6: 21     ch 11: 10
ch  3:  3     ch  7:  2     ch  5: 2, ch 8: 7, ch 9: 3, ch 10: 1
```

They cluster on 1, 6 and 11, as 2.4 GHz deployments do — those are the three
that do not overlap each other. `59:50` was dealt 3, 6 and 9, so it holds the
busiest channel in the room outright: 21 of its 28 rows are channel 6 and
legitimately its own. The other two hold the channels *either side* of 6 and
pick it up as bleed. Whether a node looks disciplined or leaky is decided by
whether the deal happened to give it 1, 6 or 11.

So the per-node figure is not a property worth tracking. The fleet-level one is,
because it is the one that costs anything:

| | access points found by more than one node |
| --- | --- |
| Run E, 2 nodes | 23 of 101 — 23% |
| Run G, 3 nodes | 22 of 95 — **23%** |

Unchanged from two nodes to three. A third node splits the same overlap three
ways rather than adding more of it, which makes sense: an access point on
channel 6 is heard by whoever holds 6 and by whoever holds its neighbours, and
that is two or three nodes either way.

**One stray was two channels out**, not one — `59:50` reporting a channel 11
access point while parked on 9. That is the geometry rather than an exception:
2.4 GHz channels are 5 MHz apart and 20 MHz wide, so a channel occupies roughly
±2 channels of spectrum and a receiver two channels away still overlaps half of
it. Runs E and F happening to show only ±1 was the room, not a rule. Nothing
was seen at ±3 or beyond, and no 5 GHz observation has landed outside a mask in
any of the six captures.

## The capability token, and a fleet that is half strangers

Run F found a stock node in the fleet by accident and showed what it costs: it
acknowledged an assignment it could not decode, kept scanning all forty
channels, and took eleven channels of the pool out of coverage while the fleet
table said it held them. The remedy was specified in the Phase 2 plan and had
not been built. It is built now.

Every heartbeat carries an ASCII token in the text field a stock node leaves
empty (`src/WiFiOps.cpp:1456-1457`): `wartui/0.1;ble,5g` — protocol version,
then what the build can do. `FleetEngine::is_assignable` refuses a node without
one, alongside the encrypted and peer-refused cases that were already there for
the same reason.

Run H is the mixed fleet, which the bench produced without being asked: `57:84`
carries the token and `59:50` is still on the build before it, sending the empty
text field that is byte-for-byte a stock node's. (Since 2026-09-10 an older
build is not byte-for-byte anything: it is recognised by its wire version and
counted as `incompatible`, which is a fault-box line rather than a table row.
The shape of the failure — flash the host, forget the nodes, get a fleet that
does nothing — is unchanged, and so is the reason it has to be visible.)

```
pool US  auto — 1 of 2  session 00:01:49  channel 6  peers 4

node              rssi beats obs  last beat  ble  channels          state
02:00:5E:10:57:84 -39  35    96   1s   4.7s       34: 1-11,36-165   rebooted x1
02:00:5E:10:59:50 -52  67    0    0s   1.8s       unassigned        not wartui
```

**`auto — 1 of 2`.** The pool was cut for one node, and that node holds all
thirty-four channels. Before the token this fleet would have been cut in two and
half the pool would have gone unscanned, with nothing anywhere saying so; the
run F measurement is exactly that failure. The stranger is in the table, named,
with its heartbeats counted — ignored is not the same as hidden.

The store agrees, and now records what each node said it was:

| node | token | heartbeats | assignments | observations |
| --- | --- | --- | --- | --- |
| `57:84` | `wartui/0.1;ble,5g` | 36 | 3 | 97 |
| `59:50` | none | 70 | **0** | 0 |

Seventy heartbeats and not one assignment, which is the whole intent. A node row
with heartbeats, no token and no assignments is a complete diagnosis months
later, which is what the store is for.

`wartui sniff` says it without a capture at all:

```
 350005505us  02:00:5E:10:57:84  -39dBm  bcast  Heartbeat #37  wartui/0.1;ble,5g
 351334537us  02:00:5E:10:59:50  -53dBm  bcast  Heartbeat #890
# 17 frames: 0 text, 17 heartbeat, 0 admin, 0 other, 0 undecodable
# 12 of 17 heartbeats carried no wartui token. `run` will not plan for those
# nodes: they acknowledge an assignment and then discard it, so a share cut for
# one is a share nobody scans. Flash them with firmware/node.
```

**The cost of this is that flashing a host without flashing its nodes leaves a
fleet that does nothing.** That is the intended trade — a fleet that visibly
does nothing beats one that silently covers half of what it claims — but it is
a real operational edge and both READMEs say so.

### What the two feature words now do

They were parsed and displayed and nothing else when the run above was taken.
Both are acted on now, and both have since been run on a bench — see "A mixed
fleet is dealt by band" and "Losing the feature is a withdrawal".

`5g` gates the planner. A node without it is dealt no 5 GHz index, because the
alternative is the run F failure reached through a node that is genuinely ours:
it adopts the share, acknowledges it, and scans the part it can tune while the
rest goes uncovered. A mixed fleet is dealt the 5 GHz half first, which is not
cosmetic — in pool order one C5 and one C6 on the US pool split 29/5, and
dealing the constrained channels first gives 23/11 for the same coverage. When
no node present has a 5 GHz radio, those channels are named in the footer rather
than dealt to somebody who would ignore them.

`ble` gates the keypress. `b` on a node built without the feature is refused in
the view and again in the engine, and a node already named as the holder loses
it on the tick that learns it cannot scan — the flag would otherwise be adopted
and acknowledged by a build with no scan code in it, so the `ble` column would
name a holder while the export had no Bluetooth rows. Losing it is an
assignment re-issued without the flag rather than a host-side record cleared:
the flag lives in the frame and the fleet table reads it there, so clearing
only the record would leave a node showing `ble` that `b` could no longer turn
off. Reflashing a node is the way to reach this, and a reflash is a reboot, so
the withdrawal rides in the frame the reboot re-issue was sending anyway.

Neither was reachable without a second kind of board, and there was not one on
the bench until runs I to K: `--sim-c6 N` makes that many simulated nodes
ESP32-C6s.

## A mixed fleet is dealt by band, and the C6 takes none of 5 GHz

Runs I to K are the first with two kinds of board in front of the planner. The
capability token is the whole of how the host tells them apart, and it says so
in plain text before anything has been planned:

```
   2515249us  02:00:5E:10:00:08  -46dBm  bcast  Heartbeat #884  wartui/0.1;ble
   2956346us  02:00:5E:10:C5:B8  -37dBm  bcast  Heartbeat #965  wartui/0.1;ble,5g
```

Run I is three nodes, one C6 and two C5s:

| node | radio | index | channels |
| --- | --- | --- | --- |
| `4F:98` | C5 | 0 of 3 | 12: `36,44,52,60,100,116,124,132,140,149,157,165` |
| `4F:08` | C5 | 1 of 3 | 11: `40,48,56,64,112,120,128,136,144,153,161` |
| `00:08` | C6 | 2 of 3 | 11: `1,2,3,4,5,6,7,8,9,10,11` |

The C6 is dealt the 2.4 GHz half entire and not one 5 GHz index; the two C5s
interleave the twenty-three 5 GHz channels between them. Pairwise overlap is
zero and the union is exactly the thirty-four of the US pool. Dealing the
constrained half first is what produces this. A naive pass in pool order would
hand the C6 every third index, most of them 5 GHz channels it cannot tune, and
the part it could not reach would be a share nobody swept — the failure the
token exists to prevent, arriving through a node that is genuinely ours.

This is also the first confirmation that a C5 hears 5 GHz here at all. The fleet
reported on 36, 40, 44, 48, 56, 116, 149, 157 and 161, which includes **DFS 56
and 116** — a node that only ever listens is welcome on those, and the stock
firmware's active scan is why the host could never make that true before.

Eighteen of the thirty-four assigned channels produced nothing. That is the
room: most of 5 GHz is empty here, and an assigned channel with no access point
on it is indistinguishable from one nobody swept until the membership check
below says which.

## Five nodes, four nodes, and a re-cut that touches everybody

Run J put all five on the air at once — the first fleet here above three, and
the first whose shares cannot come out within one of each other.

| node | radio | index | channels |
| --- | --- | --- | --- |
| `4F:98` | C5 | 0 of 5 | 8: `36,48,60,112,124,136,149,161` |
| `4F:08` | C5 | 1 of 5 | 8: `40,52,64,116,128,140,153,165` |
| `C5:B8` | C5 | 2 of 5 | 7: `44,56,100,120,132,144,157` |
| `75:40` | C6 | 3 of 5 | 6: `1,3,5,7,9,11` |
| `00:08` | C6 | 4 of 5 | 5: `2,4,6,8,10` |

Eight, eight, seven, six, five. The union is the pool and the overlap is zero,
but the shares differ by three rather than by one: two C6s have eleven channels
to divide and three C5s have twenty-three, and no arrangement makes those equal.
`plan_for` minimises the largest share instead of equalising them, and this is
the first fleet on which those two rules would have given different answers.

Pulling `C5:B8` off the bench mid-capture re-cut the plan:

| node | index | channels |
| --- | --- | --- |
| `4F:98` | 0 of 4 | 12: `36,44,52,60,100,116,124,132,140,149,157,165` |
| `4F:08` | 1 of 4 | 11: `40,48,56,64,112,120,128,136,144,153,161` |
| `75:40` | 2 of 4 | 6: `1,3,5,7,9,11` |
| `00:08` | 3 of 4 | 5: `2,4,6,8,10` |

All four took a fresh epoch, **including the two C6s whose channel sets did not
change at all**. `node_index` and `node_count` travel in every assignment and
drive each node's transmit stagger, so a fleet change re-cuts the whole plan
rather than patching the hole it left. That had only ever been a host test.

The re-cut fired 52.4 s after the unplugging was noted at the keyboard, against
a 60 s `topology_timeout`. The shortfall is the note, not the engine — the board
was pulled by hand and the time written down afterwards — so this run is
consistent with the timeout and does not measure it.

Sweep periods track the shares across both halves of the run, on the same boards
either side of the re-cut:

| node | 5-node | measured | 4-node | measured |
| --- | --- | --- | --- | --- |
| `4F:98` | 8 ch | 1.363 s | 12 ch | 1.891 s |
| `4F:08` | 8 ch | 1.386 s | 11 ch | 1.789 s |
| `75:40` | 6 ch | 1.134 s | 6 ch | 1.122 s |
| `00:08` | 5 ch | 1.032 s | 5 ch | 1.026 s |

Four more channels cost `4F:98` 0.528 s against the 0.500 s four dwells predict;
three cost `4F:08` 0.403 s against 0.375 s. The two C6s, whose shares did not
move, did not move. Every figure sits 0.08 to 0.11 s above
`channels x 125 ms + 300 ms`, which is the hop back to the control channel and
the report after each dwell.

## Membership holds across a re-cut, once the question is asked correctly

Runs I and J produced 108 observations between them and not one names a channel
outside the share the reporting node held **at the moment it reported**. The
qualifier is the whole of it. Comparing each row against its node's *final*
share instead flags three innocent ones in run J — `4F:98` reporting 48 and 161,
`4F:08` reporting 116 — all of them channels from the five-node deal, held
legitimately until the re-cut took them away. A membership check that ignores
time will manufacture faults on any run where the plan changed.

Four rows in run J name channel 6, which no node held. Those are the bleed of
"Adjacent channels bleed, and the deal is what exposes it": the channel written
down is the beacon's own DS Parameter Set, and a node dwelling on 5 or 7 hears a
channel 6 access point through the overlap.

Run I came out with duplication at zero — 67 records, 67 distinct BSSIDs —
against the 23% measured at two and three nodes above. That is structural and
agrees with what the bleed section concluded: overlap lives in 2.4 GHz, and in
run I the whole 2.4 GHz half sits on one C6, so no second node is positioned to
hear it twice. Run J was also zero, at 41 records, but that one proves nothing:
three of its four nodes had been assigned in run I already and were reporting
through a `MacRing` that still held everything they had seen.

## Losing the feature is a withdrawal, not a forgotten name

Run K is the case the `ble` gate was written for, and the one that needed a
second kind of board to reach: a node holding the Bluetooth scan whose next
heartbeat says it has no code for it.

`00:08` was given the scan by hand and took it — flag set, acknowledged, 67
advertiser records within two seconds. It was then reflashed mid-capture with
`ble` left out of the features, which is the only way a token loses a word.

```
t= 14.3s  00:08  epoch 4  ble=1  acked
t= 88.8s  00:08  epoch 5  ble=0  acked
t= 88.8s  heartbeat counter 787 -> 1   (reboot)
```

One epoch, at the moment the reboot was detected, carrying the flag cleared. Not
a re-issue at the old flag followed by a second epoch taking it away: the
withdrawal rode in the frame the reboot re-issue was sending anyway, which is
what the host test asserts by checking the node is not left dirty afterwards.

That the flag byte changed is not by itself proof the scan stopped, and the
advertiser stream cannot supply it — those records had already decayed to
nothing at t=62.8 s, twenty-six seconds before the reflash, because `MacRing` is
shared with Wi-Fi and never cleared. The reboot is what makes the question
answerable, because a reboot empties that ring. The Wi-Fi stream shows it
emptying: twenty-six records in the ten seconds after the reboot against one to
four in every neighbouring bucket, the node re-reporting a neighbourhood it had
already reported once. Bluetooth records in that same window: none, and none in
the remaining 190 s. A node still scanning would have re-reported all
sixty-seven.

## The Bluetooth cost is the same tenth, and the median can hide it

`00:08` held eleven channels throughout run K, so the same node on the same
share can be compared with the scan running and with it compiled out.

| | sweeps | mean | median | p90 | max |
| --- | --- | --- | --- | --- | --- |
| scan on | 29 | **1.914 s** | 1.754 s | 2.277 s | 2.279 s |
| no `ble` build | 107 | **1.751 s** | 1.750 s | 1.753 s | 1.762 s |

**+9.35% by mean**, which agrees with the +10.9% measured on a C5 above. By
median it is +0.2% — and the median is what that earlier measurement used.

It was the right statistic there and the wrong one here, for a reason worth
writing down. `BLE_INTERVAL_MS` is `NUM_SCAN_CHANNELS * CHANNEL_DWELL_MS`, five
seconds, and it does not shrink with a node's share. The C5 in run B held all
thirty-four channels and swept in 5.2 s, so a scan landed on essentially every
sweep and every sweep carried the cost. `00:08` holds eleven and sweeps in
1.75 s, so a scan lands on about every third one: nine of twenty-nine sweeps ran
around 2.28 s and the other twenty were untouched. Nine sweeps 0.53 s longer
across 55.5 s is the 9.35%.

So the cost is about a tenth of a node's sweeping whatever its share, but on a
small share it arrives as periodic sweeps a third longer rather than as a
uniformly slower one. The `beat` column shows that as jitter, and any summary
taking the middle of the distribution reports no cost at all. The separation
itself is not marginal: the loaded p90 is 2.277 s and the unloaded **maximum**
over 107 sweeps is 1.762 s.

## Still not measured

- **A uniform fleet above three nodes.** Run J reached five nodes and then
  four, but both fleets were mixed, so the shares were forced by the radios
  rather than by the arithmetic. The case where thirty-four splits 9/9/8/8 — four nodes that
  can all tune everything, two of them taking the remainder — needs four C5s,
  and this bench has three.
- **Boards that stay up.** Two of the three refused `espflash` at some point in
  this session and one had to be flashed by hand; a `reset` immediately followed
  by a `monitor` left another answering `MemData command / Other (0x1)` and it
  missed a whole capture. Phase 1 blamed a host USB port for the same symptom,
  which was true of that instance and is not the general rule — these boards are
  simply not reliable about it. The second bench added a case: the bridge came
  up silent, answering nothing on the link protocol and emitting not one byte —
  not even the undecodable stream a board running node firmware produces — and
  one `espflash reset` fixed it. Nothing here is a firmware result, but it is
  why a run should check that every board it expects actually booted, the
  bridge included.
- **The stagger, again.** Unchanged from Phase 1: it cannot be disabled from the
  host, so "it worked" and "there was nothing to prevent" remain inseparable
  without a firmware build that omits it.
- **Duplication beyond three nodes.** It is 23% at two and 23% at three, which
  is the number that costs store rows. Whether it stays flat at ten is a guess:
  the argument that it should — an access point is heard by whoever holds its
  channel and whoever holds the neighbours, which is two or three nodes however
  many there are — is reasoning, not a measurement. Run I's zero neither
  contradicts nor extends it: a mixed fleet small enough to put all of 2.4 GHz
  on one node has no second node positioned to hear the bleed, which is a
  property of that shape rather than of the node count.
- ~~**Whether a version mismatch should be a refusal.**~~ *Answered 2026-09-10.*
  The incompatible change happened, and the answer is that it is neither obeyed
  nor ignored: the frame header carries a wire version byte of its own, and one
  this build does not know is counted as `incompatible`, kept out of the fleet
  table, and named in the footer as `N frames from an older firmware — reflash`.
  A half-flashed fleet is the situation that policy exists for, and it is the one
  case where saying nothing would look exactly like a fleet that had gone quiet.
- **A C6-only fleet, and the shortfall in the footer.** The mixed case is now
  measured (runs I to K), but a fleet with no 5 GHz radio in it at all is not.
  What should happen is that the twenty-three unreachable channels come back in
  `Plan::unreachable` and are said in the footer rather than dealt to somebody
  who would ignore them. Every run here had at least one C5 in it.
- **A fleet with more nodes than it has channels it can reach.** Twelve C6s on
  the US pool have 11 dealable channels between them, so at least one node is
  dealt nothing — and there is no frame meaning "scan nothing", so it keeps
  whatever it last held and doubles up on somebody else's share. That used to
  need more than 34 nodes and so more than the peer table allows; with the
  radios read out of the tokens it is inside a fleet the bridge can address.
  Nothing on screen says which node it is: the footer names the unscanned
  channels, not the duplicated ones. Whether that is worth a fault line is
  undecided, and the fleet it needs has never existed here.
- **What one mangled heartbeat would cost.** A node's token is re-read from
  every heartbeat and nothing smooths that, so a single heartbeat whose text did
  not parse would take the node out of the plan, re-cut and re-epoch the entire
  fleet, and be undone by the next one. Two checksums stand between the air and
  that text, so it has never been seen and may not be reachable; it is written
  down because the failure would look like an unexplained fleet-wide re-cut
  rather than like a bad frame.
- **A dense room.** The Bluetooth penalty was measured where about fifty
  advertisers answered each scan. Whether it stays near a tenth of a sweep where
  five hundred do is unknown, and this bench cannot produce that.
- **Whether channel 14 is genuinely unreachable on hardware.** Phase 2 removed
  it from every pool on the strength of the Phase 1 measurement and the
  `esp-radio` country blob, and no run since has tried to tune it. It is now
  unreachable by construction from the host, which is the intended state but
  also means the original refusal is no longer being re-checked.
