# Phase 4 — a wire of our own

Written 2026-09-10, before the bench; the measurements were added the same day,
after it. The reasoning below was written down first on purpose, so that what
the hardware said could be compared against what the change claimed rather than
summarised alongside it.

`file:line` citations below point into
[wowy/ESP32DualBandWardriver](https://github.com/wowy/ESP32DualBandWardriver) on
`feat/node-interference-mitigation`, the vendor firmware.

The bench was one ESP32-S3 bridge (`6A:4C`) and three ESP32-C6 nodes (`75:40`,
`9A:24`, `00:08`), `9A:24` flashed with the `ble` feature and the other two
without. **No ESP32-C5 was attached**, so nothing here exercises 5 GHz: every
run carried the footer saying 23 channels of the US pool went unscanned for want
of a radio that could reach them.

## What changed, and why it is not a tidying-up

Until now node → core was byte-identical to the vendor firmware's. Phase 2 had
already made core → node wartui's own, so the wire was half ours and half
inherited — and the half that was still shared was the half that could not
safely be.

ESP-NOW has no addressing above the MAC layer and a node broadcasts to
`FF:FF:FF:FF:FF:FF`, so two fleets speaking one format are in one conversation.
Both directions of that were live, and Phase 2's own bench measured one of them
without recognising it as symmetric:

- **Ours mis-planned theirs.** Run F of `docs/phase-2-findings.md` is a vendor
  node in a wartui fleet: it heartbeated, was planned for, acknowledged its
  assignment at the MAC layer, discarded it in the application, and kept
  scanning all forty channels while the share cut for it went unscanned — two
  nodes and a stranger covering less of the pool than the two alone. Read from
  the other side, that is exactly what a wartui node did to a vendor core in
  range: vendor-shaped heartbeats going into its table, its plan cut around a
  node that would never obey it.
- **Ours corrupted theirs.** Worse, and not measured — found by reading the
  vendor source while planning this change. Its receive handlers test
  `if (len < sizeof(...)) return;` rather than for equality, so wartui's
  fourteen-byte assignment cleared a stock node's ten-byte check and its leading
  bytes decoded as `enow_admin_msg_t`: epoch, index and count landing correctly,
  and then the flags byte read as `start_channel_idx` and the first mask byte as
  `end_channel_idx`. **A stock node in range adopted a garbage channel range
  from us.** `air::is_legacy_admin` was exact on length in our direction; the
  vendor was never exact in its own.

So the frames are ours in both directions now, behind a `WTUI` magic checked
before anything else at either end, and a wire version byte the vendor header
never had.

| Frame | Was | Is |
| --- | --- | --- |
| Heartbeat | 212 | 13 |
| Sighting, no SSID | 212 | 17 |
| Sighting, 32-byte SSID | 212 | 49 |
| Assignment | 14 | 15 |

The padding is the reason the first three moved so far. Every text frame was
`sizeof(enow_text_msg_t)` whatever it carried, so a heartbeat with an empty
payload and an observation of a hidden network cost the same 212 bytes on the
control channel every node in the fleet has to share.

Two things fall out that are worth measuring for their own sake:

- **SSIDs stop being mangled.** The vendor line was split on commas, so the
  sender rewrote a comma inside an SSID as an underscore before transmitting
  (`src/WiFiOps.cpp:1768`) and the real name was lost at the one point in the
  path where it still existed. The SSID is length-prefixed now, and `export`
  already quotes CSV correctly at the only boundary that is actually CSV.
- **A half-flashed fleet says so.** `wartui-proto` is compiled into the host and
  both firmwares, so a wire change means flashing every node at once. A node on
  the older build is counted as `incompatible` and named in the footer rather
  than vanishing, which is what the version byte is for. This also answers the
  question `docs/phase-2-findings.md` left open under "Still not measured".

  This change is the one case that byte cannot cover, because the frames that
  precede it predate the header carrying it — and a live bench made that
  concrete. `wartui` was started against the boards on the desk while this was
  being written, and the footer said `1 frames from a vendor fleet` about a node
  that is one of ours, a foot away, waiting to be flashed.

  Nothing was built to fix that, deliberately. A pre-1.0 wartui gets no
  compatibility from a later one, so a node on the older build is exactly what
  the footer says it is: traffic this host cannot read, indistinguishable from a
  neighbour's because it *is* the vendor's format. The answer is to flash the
  fleet, not to teach the host a format it is trying to stop speaking.

## What the bench answered

1. **The frame lengths are real.** `wartui sniff --raw` on channel 6 put a
   heartbeat on the screen as thirteen bytes and no more:

   ```
   57 54 55 49  01  01  5d 00 00 00  01 00 00
   "WTUI"      ver typ  counter=93   maj min flags
   ```

   Sightings measured 17 bytes for BLE (n=60, every one of them, the SSID being
   empty by construction) and 17 to 33 bytes for Wi-Fi (n=38, mean 25.3), the
   spread being the SSID and nothing else.

2. **Air time per sweep fell to between a fifteenth and a thirteenth.** Counting
   every frame a node actually put on the air across two settled 65–70 s
   captures, and comparing against what the same traffic would have cost at the
   old fixed 212:

   | Node | Frames | Ours | At 212 bytes | Share |
   | --- | --- | --- | --- | --- |
   | `9A:24` beats only | 82 | 15.4 B/s | 250.7 B/s | 6.1% |
   | `75:40` beats + 20 sightings | 98 | 21.7 B/s | 296.5 B/s | 7.3% |
   | `75:40` beats only | 73 | 14.5 B/s | 236.5 B/s | 6.1% |
   | `9A:24` beats + 78 sightings (BLE on) | 151 | 37.1 B/s | 491.0 B/s | 7.6% |

   This is payload, not occupancy — the caveat the pre-bench note made still
   stands, because ESP-NOW wraps each frame in an 802.11 action frame whose
   header and preamble do not shrink with it. The floor on the saving is
   therefore lower than 6%. It is not higher.

3. **Adoption latency did not get worse.** Measured host-side from the
   assignment being handed to the link to its outcome coming back, because the
   bridge-stamped figure was not trustworthy at the time (below): 2 ms at best,
   4–7 ms median in a settled session, 55 ms at worst during a startup burst.
   Once the backlog bug below was fixed the bridge-stamped figure became usable
   and agrees: 2201–4203 µs, against the 5832 µs Phase 2 measured on a node that
   was also scanning Bluetooth. Nothing here suggests a shorter frame costs
   anything.

4. **Neither fleet reached the other — one direction on the bench, one by
   construction.** A fourth C6 was flashed with the bridge firmware and used to
   broadcast vendor-shaped frames on channel 6 at two per second: a ten-byte
   `ENOW` type-5 assignment and a 212-byte `ENOW` node frame. The host counted
   them as `49 admin frames from another core` and `49 frames from a vendor
   fleet`, admitted neither to the node table, and — the point of the whole
   change — **neither node moved**: `75:40` held `1,3,5,7,9,11` and `9A:24` held
   `2,4,6,8,10` throughout, while an assignment naming channels 0 through 5 was
   on the air beside them. The node firmware said nothing about those frames at
   all, which is correct: a bad magic returns before anything is decoded.

   The other direction — a stock node not hearing *us* — was **not** put on a
   bench, because no vendor device was available. It rests on the vendor's
   handlers doing `memcmp` on the magic first, which is read from its source
   rather than measured. Worth doing if a stock board ever turns up; the failure
   it would catch is the one that mattered most.

5. **The reflash boundary works from both ends.** Injecting `WTUI` with wire
   version 9 produced `31 frames from an older firmware — reflash` in the
   footer, no node-table entry, and no half-decode. The same frames reaching a
   node made it say so on its own console:

   ```
   ignoring a frame at wire version 9; reflash this node
   ```

   which is the whole of what the version byte was added for.

## What else the bench showed

- **Reboot detection survived the change.** A single `espflash reset` of one
  node moved it to `rebooted x1` and left the other alone, and the store shows
  exactly one counter regression (391 → 1) to back it.
- **The node's dedup ring is why a second capture looks empty.** A freshly
  flashed node reported 42 networks in its first session and none in the next
  three; the node that had just rebooted reported 20 while the one that had not
  reported zero. The ring lives on the node and outlives a host session, which
  is correct and is worth knowing before reading a quiet fleet table as a fault.
- **A three-node fleet cut round-robin as designed**: `1,4,7,10` / `2,5,8,11` /
  `3,6,9`, all three alive and assignable.
- **`Security` survived the trip through a byte.** An observation beaconed as
  WPA2 left the node as a discriminant and arrived in the CSV as `[WPA2_PSK]`.
- **The comma property is not bench-verified**, because no access point in range
  beacons a comma. Driven through the simulator instead, an SSID of
  `Bob, Alice "and" Co` reached the CSV as `"Bob, Alice ""and"" Co"` — quoted,
  both characters intact, where the vendor line would have delivered
  `Bob_ Alice`. The encoding half of that is pinned in
  `crates/wartui-proto/tests/wire.rs`.

## Found on the bench, fixed after it

Two things turned up that predated the wire redesign — the diff that broke the
wire touched neither the send path nor the latency computation — and they turned
out to be one bug wearing two faces.

The two symptoms were:

- **`assignment.latency_us` was not measuring what its doc comment said.** It
  reported whole seconds where the host's own round trip was milliseconds —
  8.0 s, 7.6 s, 6.9 s on a bridge that had been powered for three seconds — and
  it *decreased* across a burst of sends, by roughly one heartbeat interval each
  time, which no elapsed-time quantity can do. The u32 microsecond wrap at
  71.6 minutes was the obvious suspect and was ruled out: resetting the bridge
  to a fresh uptime changed nothing.
- **An assignment went out nine times inside 250 ms.** One node was sent the
  same epoch nine times in a quarter of a second at session start, eight of them
  unacknowledged, while the fleet table showed it holding the assignment the
  whole time.

Neither was a retry loop, because there is no retry loop: `send_admin` has one
caller, the heartbeat handler. Nine sends meant nine heartbeats, and the 1.03 s
decrement was the interval between them. **The host was working through a
backlog and could not tell.**

The bridge buffers what it hears while nothing is attached — its outbox rings
are the whole reason a frame survives a slow host — so the first thing a fresh
connection receives is a ring's worth of the recent past, delivered as fast as
USB will carry it. A probe against the bridge clock measured it: twenty-five
frames spanning **eight and a half minutes** of bridge time arrived inside
**seventeen milliseconds** of host time, the oldest 532 seconds stale. From
frame 25 on, the bridge's stamps and the host's own advanced in step to within
130 µs.

Every one of those replayed heartbeats named an admin window that had shut
minutes ago, and the engine opened one for each. The transmits went to a node
that was off sweeping a channel where it could not hear them, and the "latency"
was the true distance from a heartbeat in the deep past to a callback in the
present.

The fix is in the engine, where it can be tested against an invented clock.
Nothing in a frame says how old it is, but the bridge stamps every one and the
two clocks tick at the same rate, so the comparison is free: while the link is
read live the bridge's stamps advance in step with the host's, and while a
backlog drains they run far ahead. `FleetEngine::note_arrival` accumulates that
gap and resets it the moment the host spends longer waiting for a frame than the
bridge spent producing one, which can only happen when nothing is queued. A
connection starts by assuming it is behind, since the first frame of a backlog
is the one stale frame no comparison can catch; that costs at most one window on
a quiet fleet, and the next heartbeat is a second away. `latency_us` is then
`None` rather than a fabricated number whenever the figure exceeds the window it
claims to measure.

Measured on the same bench, connecting to a bridge that had been buffering for
29 minutes:

| | Before | After |
| --- | --- | --- |
| Assignments sent | 13, nine of them one epoch in 250 ms | 5, each a distinct epoch |
| Unacknowledged | 8 | 1 |
| `latency_us` | 80 s, 12.5 s, 11.4 s, … | 2285, 2260, 3256 µs |

A second connection straight afterwards reported `admin 4/4` with no
unacknowledged assignments at all, and latencies of 3647, 2201 and 4203 µs —
the same order as the 5832 µs Phase 2 measured, and for the first time a number
that means what the column says it does. Assignments held back for a live window are counted as
`admin_windows_missed` and said in the footer beside the admin totals — only
where one was actually owed, so the figure is transmits deferred rather than
one per stale frame, and it is the answer to the question a fresh connection
otherwise raises: why nothing has been assigned yet.

## Still not measured

- **Whether the shorter frames change the dedup ring's usefulness.** The ring
  exists because a node that reported every access point on every dwell would
  flood the control channel. Cheaper frames raise the flood ceiling; whether
  that is worth spending on a shorter ring, or on reporting a strong signal
  twice, is a question this change makes askable and does not answer.
- **Whether `Security::Unknown` ever arrives.** It exists so a node from a later
  build cannot lose an observation to a mode this host has no name for. Nothing
  has produced one, and nothing will until the two ends are deliberately skewed.
- **Anything at 5 GHz.** No C5 was attached, so `Radio::tunable`, the
  constrained-half deal and `Plan::unreachable` were exercised only in the sense
  that all 23 5 GHz channels went undealt and were named in the footer, every
  run. A mixed C5/C6 fleet remains the interesting untested case.
- **A stock device in the room.** See point 4 above: the direction that matters
  most is the direction no bench here could run.
