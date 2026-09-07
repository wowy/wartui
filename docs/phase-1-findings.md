# Phase 1 — what the node firmware actually does

Measured on 2026-09-06. The bridge throughout is an ESP32-C6 running
`firmware/bridge` (`9D:24`, USB). Everything up to "Every connection
begins with one undecodable frame" used one ESP32-C6 node
(`00:08`, `esp32c6` feature); the 5 GHz section that follows used
one ESP32-C5 (`57:84`, `esp32c5` feature) in its place — one node at
a time, so none of that is a two-node result. "Two nodes split the pool" is, and
used two C5s at once: `4F:98` alongside that same board, as does
"BLE coexists", which is the only run with Bluetooth compiled in — and then into
`57:84` only. Plaintext, control channel 6.

Access point addresses and names are left out deliberately: these were real
captures of a real neighbourhood, and a BSSID is exactly what a geolocation
database is built from. Counts and channels are the whole of what was being
checked, and they carry none of that.

## Sniffing and ESP-NOW do coexist

The design's largest open risk — that promiscuous mode would starve the ESP-NOW
receive path, and that the whole `sniffer()`/`esp_now()` shared-borrow shape
would have to be abandoned for `scan_async` — did not materialise. A node
holding both for the life of the program reported observations and acknowledged
assignments in the same sweep, repeatedly, across three captures — one `sniff`
and the two `run` sessions the rest of this document draws on.

## An unassigned node parks and waits, as designed

Before any assignment the node broadcast `MSG_HEARTBEAT` every 1005–1013 ms with an
empty text field and a counter monotonic from boot, and collected nothing. That
is the deliberate divergence from `WiFiOps.cpp:77-80`, where the vendor default
is all forty channels, and it is what the node README warns costs you a capture
under `--manual` until a key is pressed.

## Assignments are acknowledged, in single-digit milliseconds

Every `MSG_ADMIN` sent in these sessions came back `SendStatus::AckOk`. This is
the failure Phase 0 exists to document, inverted:

| | Phase 0, vendor node | Phase 1, this firmware |
| --- | --- | --- |
| Admin frames sent | 32 | 8 |
| Acknowledged | **0** | **8** |

No sniffer was running, so there is no sequence-number or retry-bit column to
set against Phase 0's; the acknowledgement itself is the measurement, and it
comes from the receiver's MAC hardware either way.

Delivery, measured host-side from `created_at` to the transmit callback, was
2–11 ms. The two assignments with no queue behind them — the first hand
assignment, and the re-issue after a reboot — measured 6141 µs and 5908 µs
heartbeat-to-callback on the bridge's own clock, against a 300 ms admin window.

**Six identical assignments went out in one 9 ms burst at connect.** This is
host behaviour, not node behaviour, and it is benign: a bridge left powered
buffers heartbeats, so `run` attaching to one that has been up for half an hour
processes a backlog of them before any acknowledgement can return, and emits an
assignment per heartbeat. All six carried epoch 1, the node adopted the first
and discarded five on the `!=` rule, and all six were acknowledged.

That last part proves less than it looks like it does, and the first draft of
this document claimed otherwise: it argued no stock node could have heard six
frames sent inside 9 ms. It could have. A 9 ms burst is far more likely to fall
wholly inside a 300 ms window than to straddle it, so a stock node would hear
all six or none — the burst separates nothing. And epoch 1 is by definition the
first assignment, so the node was still unassigned and parked on the control
channel, listening continuously for reasons that have nothing to do with the
sniffing design. The evidence for that claim is the sustained observe-and-
acknowledge behaviour above, not this.

The six frames' `latency_us` reads 7.4–9.6 s because the paired
heartbeats genuinely were that old; the metric is correct and the backlog is
what it is measuring. It is bounded by the bridge's ring and does not recur in
steady state.

## Sweep period, with the caveat that makes it not a fair fight

This firmware beat every 1.7 s across channels 1–11, and every ~430 ms on a
single channel.

There is no vendor number to set either against. Phase 0 measured 6.7 s for a
**forty**-channel sweep and predicted about 3.6 s for one channel; it never
measured eleven, because the run that would have — the halved channel count —
produced nothing at all, the assignment having never arrived
(`docs/phase-0-findings.md:106-114`). So the only honest comparison is the
one-channel pair, ~430 ms against a predicted 3.6 s, and even that is not
like-for-like: the vendor figure carries a per-sweep BLE scan this build does
not compile in, and Phase 0 is explicit that the overhead is fixed rather than
proportional to channel count.

What the numbers do establish without a comparison is that the 300 ms
admin window is being honoured and is the dominant cost at a narrow
assignment — 125 ms of dwell against 300 ms of listening — which is the shape
the design intends and the reason a one-channel node is the case the BLE rate
limit had to be safe for.

## Channel membership holds

The check `docs/phase-0-findings.md:116-118` recommends over heartbeat period.
Every observation named a channel inside the range the node had been assigned,
in both `run` sessions and with nothing outside:

| Session | Assigned | Channels observed |
| --- | --- | --- |
| Hand assignment, `a` (narrowest) | index 0 (channel 1) | 1, and only 1 |
| Planner, US pool | indices 0–10 (channels 1–11) | 1, 3, 6, 9, 10, 11 |

The narrow case is the load-bearing one. A node whose `set_channel` silently did
not stick would report the right access points against the wrong frequency, and
the only symptom would be observations on channels it was never given.

## Reboots are detected and the assignment is re-issued

`espflash reset` on the node mid-capture, with the host untouched: the counter
restarted, the fleet table showed `rebooted x1` within a second, and the host
re-cut and re-sent the assignment with epoch 2, acknowledged in 5.9 ms. The
node's dedup ring cleared with the reboot, as it must — it is `static` and never
cleared at runtime — and it re-reported 31 access points within fifteen seconds,
all of which it had already sent before the reset. That is the intended cost of a reboot, not a fault.

## Every connection begins with one undecodable frame

Every session saw exactly one. `run` counts it in the fault box as `garbled 1`
(`tui.rs`, fed by the engine's `LinkEvent::Garbled` tally); `sniff` prints it as
a `# undecodable frame: …` line and does **not** count it in its own summary,
whose `undecodable` column is a different measurement — `Frame::decode`
failures on the ESP-NOW payload inside a frame the link decoded perfectly well,
which is the encrypted-node symptom rather than link resync. The two are worth
keeping apart; a session can and did read `0 undecodable` while printing one
undecodable-frame line. It is always the same thing, and always immediate:

```
18:51:18.402464Z DEBUG undecodable frame error=checksum mismatch: expected 0xa62c, got 0x4634
18:51:18.405006Z  INFO the bridge announced itself
```

Half a millisecond after the port opens, and before the bridge announces itself
— the tail of a frame that was already in flight when the host attached. The
framing resynchronises immediately and nothing is lost but that one frame, so
this is a counter and not a fault, exactly as `serial.rs` says.

It is recorded here because it was very nearly worse than that. The
five-second "nothing has identified itself as a bridge" notice was first written
to stay quiet whenever the link had said anything at all, `Garbled` included —
which on this hardware meant it was suppressed within half a millisecond of
every run it could ever have fired in. The feature was dead on arrival and
nothing in the test suite could have shown it; only the wire could. It now
counts an announcement, a decoded message, or a stated reason the link is down,
and was re-verified against a bridge held in its bootloader with
`espflash board-info --after no-reset`, where it fires at five seconds with the
garbled frame present.

## The C5 does 5 GHz, and refused ten channels of it until it was told where it was

Added 2026-09-06 with an ESP32-C5 (`57:84`, the BLE-on node from
Phase 0, reflashed) as the only node, assigned the US pool's 5 GHz run — indices
14–36, channels 36–165 — by hand with `A`. The assignment was acknowledged in
6.7 ms and observations came back on channels 36, 44, 48, 56, 149 and 161. The
`BandMode::Auto` path had never been executed before; it works.

**Channel 56 is a DFS channel and it produced six observations.** That is the
result the sniffing design exists for: a node listening where a stock node would
have transmitted a probe request.

**Channels 100–144 are refused by the radio outright.** Over a 130 s capture the
node logged `radio refused channel N` sixty-four times each for 100, 112, 116,
120, 124, 128, 132, 136, 140 and 144 — every channel of UNII-2 Extended in the
scan table, every sweep, without exception, while every other 5 GHz channel
parked normally. `radio::park` reports this when `EspNowManager::set_channel`
returns `Err`, so the refusal is the driver's and happens before any dwell.

The sweep period corroborates it independently. Twenty-three assigned channels
at 125 ms would dwell for 2.9 s, and with the 300 ms admin window the beat
should have read about 3.2 s. It read 2.0 s — which is what thirteen dwells plus
the window come to, and thirteen is exactly what is left after ten refusals.

Three things follow, in descending order of how much they matter:

- **Ten of the US pool's twenty-three 5 GHz channels cannot be covered by a C5
  on this `esp-radio`.** Resolved the same day; see the section below.
- **The host cannot see it.** The planner assigns those indices freely, and a
  node given only 100–144 would sit there `alive`, heartbeating, reporting
  nothing — indistinguishable from a node whose radio silently did not tune,
  which is the failure `docs/phase-0-findings.md` warns reads as a dead node.
  The node knows and says so on its own serial console, where the host is not
  listening.
- **The firmware handles it correctly**, which is worth recording because it was
  a deliberate choice under review. A refused hop still calls `advance()`, so the
  cursor is never stranded on a channel the radio will not take, and nothing is
  transmitted or counted for a channel that was never reached.

### The cause was the regulatory domain, and the default is China

`EspNowManager::set_channel` is a bare passthrough to `esp_wifi_set_channel`, so
the refusal comes from the blob, which decides from `wifi_country_t`.
`esp-radio 1.0.0-beta.0` fills that in inside `WifiController::new` from
`ControllerConfig::country_info`, whose default is `CountryInfo::from(*b"CN")`
under `WIFI_COUNTRY_POLICY_MANUAL` — so nothing on the air ever overrides it,
and both firmwares were passing `Default::default()`.

China's 5 GHz allocation is 5150–5350 and 5725–5850. The 5470–5725 band is not
in it, and 5470–5725 is channels 100–144: exactly the ten refused, exactly the
ten not. It was never a DFS rule — 52–64 are DFS too, and channel 56 was one of
the channels that worked.

Both firmwares now pass `US`. Re-measured on the same C5, same hand assignment,
same 130 s capture:

| | Sweep period | Implied dwells | `radio refused` |
| --- | --- | --- | --- |
| `CN` | 2.031 s | 13 | 640 |
| `US` | 3.298 s | 23 | 0 |

The difference is 1.267 s, which is 10.1 dwells at `CHANNEL_DWELL_MS`. Both
figures reconcile against the 300 ms admin window: (2.031 − 0.3) / 0.125 ≈ 13.8
and (3.298 − 0.3) / 0.125 ≈ 24.0. **No observations came back on 100–144** —
nothing in this neighbourhood beacons on UNII-2C — so the evidence that those
channels are now dwelt on is the timing and the silent log, not a sighting.

### One channel is still refused, and the country code cannot reach it

A second run assigned all forty indices (`0..=39`, channels 1–177; acked in
5.8 ms, sweep period 5.351 s ≈ 39 dwells). Under `US` the C5 accepts every
channel in `SCAN_CHANNELS` including the UNII-4 channels 169, 173 and 177 — and
refuses **channel 14**, once per sweep, every sweep.

That one is not the country code. `CountryInfo::into_blob` hardcodes
`schan: 1, nchan: 13` and `wifi_5g_channel_mask: 0`, and exposes neither; the
mask is the field the blob's own header says overrides the country's table
outright when the policy is manual, which it already is. Checked against both
the pinned 1.0.0-beta.0 and upstream `esp-hal` `main`: both still hardcode all
three, and there is no `esp-config` knob. Reaching them needs
`esp_wifi_set_country` called directly, which means `unsafe` and a direct
`esp-wifi-sys` dependency — declined, and `unsafe_code = "forbid"` was added to
both firmware crates instead, which the root workspace's `exclude = ["firmware"]`
had left to documentation alone.

So the firmware is permissive on 39 of 40 channels and the pool is otherwise
entirely the host's business. Channel 14 is index 13, is Japan-only and
802.11b-only, and was outside the default `us` pool already; `--pool all` was
the only way to reach it, and the cost was one refused hop per sweep with the
cursor advancing correctly.

It was left in `ChannelPool::All` at the time, because that pool was one
contiguous run and splitting it around index 13 would have made a lone node
rotate between two runs on the 60 s dwell timer — halving what it watched at any
instant, for a channel nobody uses. **Phase 2 removed it.** The channel mask
made the exclusion cost one clear bit rather than a rotation, so `All` is now
two runs and 39 channels, and no pool offers index 13 at all
(`plan::UNSUPPORTED_INDEX`). The reason to take it out rather than leave it as
an operator's problem is what this section measured: the refusal is reported
only on a serial console, so a fleet on `--pool all` was spending a dwell of
every sweep on nothing and nothing in the host said so.

It stays in `SCAN_CHANNELS`. That table's indices *are* the wire format, and
removing an entry would repoint every assignment in flight and every stored row.

### Probe requests: argued, not captured

The claim that a listening node transmits nothing on a DFS channel is *not*
verified by a monitor-mode capture here. It is verified by construction: every
transmit in the sweep — `report` and the heartbeat both — is gated on
`on_control`, the return of the park back to channel 6, and there is no other
transmit path in the loop. The node cannot send on a dwell channel because no
code does. What a capture would still add is whether the driver itself emits
anything while parked, which this does not answer.

## Two nodes split the pool, and each stays inside its half

Added 2026-09-06 with two ESP32-C5 nodes attached at once — `4F:98`
and `57:84` — and the planner left on, so wartui cut the pool itself
rather than being told what to do. This is the first run in which
`node_index`/`node_count` were anything but 0 and 1.

The cut was predicted from `plan()` before the run and came back exactly:
`apportion` finds two runs and two nodes, so `spare` is zero and each run goes
whole to one node. `4F:98` took indices 0..=10 (channels 1–11), `57:84` took
14..=36 (36–165), both with `node_count 2`, and `phase_count` was 1 — a two-node
fleet on the US pool does not rotate.

**Channel membership holds in both directions**, which is the check phase 0
recommends over heartbeat period. Over 190 s, `4F:98` produced 75 observations on
channels 1, 3, 5, 6, 8, 9, 10 and 11 and nothing else; `57:84` produced 21 on 36,
44, 48, 56, 149 and 161 and nothing else. Neither node ever named a channel from
the other's range.

Sweep periods track the assignment sizes, and were also predicted first:

| Node | Channels | Predicted | Measured |
| --- | --- | --- | --- |
| `4F:98` | 11 | 1.675 s | 1.704 s |
| `57:84` | 23 | 3.175 s | 3.357 s |

Both reconcile as dwells plus the 300 ms admin window, and `57:84`'s figure
agrees with the 3.298 s the same board gave alone on the same run of channels.
Neither node refused a channel; `4F:98` had never been run before this, so the
regulatory-domain fix is not specific to the board it was debugged on.

**The `!=` adoption rule is confirmed on hardware.** The host sent `57:84`
assignment version 3 twice. The node's console logs two adoptions across three
frames — v3 and v5 — so the duplicate was acknowledged by the MAC and then
discarded, which is what the invariant says should happen and had never been
observed.

### Six assignments in 16 ms, and why that is three separate causes

All six assignments were created within 16 ms of the host attaching, all acked,
with `latency_us` between 8.5 and 11.4 seconds. That latency is not delivery
delay: the bridge buffers heartbeats while no host is reading, so `run`
attaching to nodes that have been powered for a while drains a backlog before
any ack can return, and the metric is correctly reporting how stale the queued
heartbeats were.

The three re-cuts inside that window have three different causes, and the
heartbeat arrival order shows all of them:

| t (ms) | Node | Counter |
| --- | --- | --- |
| 0 | `4F:98` | 47 |
| 1 | `57:84` | 42 |
| 1 | `4F:98` | 48 |
| 1 | `57:84` | 43 |
| 7 | `4F:98` | 1 |
| 16 | `57:84` | 1 |

- **v1** is `4F:98` alone: one node known, `node 0 of 1`.
- **v2/v3** are the fleet changing. `57:84` becomes a member, `replan` sees
  `members != plan_members`, and the whole plan is re-cut — the documented
  behaviour that every fleet change re-cuts everything, not just the new node.
- **v4/v5** are reboot detection. The counters at t=0–1 are from before the
  monitors attached; `espflash monitor` resets the chip on connect, so the nodes
  restarted at 1 and the host saw a counter go backwards. That is `reissue`,
  which mints a fresh epoch. The reset is an artefact of the measuring harness
  rather than anything the firmware did, and it re-confirms reboot detection on
  two nodes at once as a side effect.

The same overlap explains the duplicated counters in the store: both nodes
counted past their pre-reset values again after restarting, so `57:84` has 64
heartbeat rows for 62 distinct counters.

### What the stagger did is still not separable

`stagger_offset_ms` finally ran with real arguments — 0 ms for node 0 and 60 ms
for node 1 — but this run cannot isolate its effect. The two nodes hold
different-sized assignments, so their sweep boundaries drift past each other
continuously rather than colliding at a fixed offset, and there is no
instrumentation that would show a deferred transmit.

What can be said is that nothing was lost to contention on the faster node:
`4F:98` delivered 120 heartbeats with **no** missing counter. `57:84` lost three
of 65 — counters 13, 26 and 45, spread rather than clustered. Nothing anywhere
reports them: the host log is clean, both nodes' sighting rings dropped nothing,
and a broadcast heartbeat is unacknowledged by design, so a lost one leaves no
trace on either end. Three in 190 s is consistent with ordinary broadcast loss,
and this run cannot distinguish that from contention with the other node.

## BLE coexists, once the controller is allowed to say anything

Added 2026-09-06 with `57:84` rebuilt `esp32c5,ble` and
`4F:98` left BLE-off — Phase 0's experiment on our own firmware, and
on the same physical board that under vendor firmware acknowledged **0 of 32**
assignments. Both were hand-assigned the *same* 23-channel range, so neither the
sweep comparison nor the stagger is confounded by one node having more to do.

### First, a bug that made the first attempt meaningless

The first run of this checkpoint reported no Bluetooth at all: no advertisers, no
BLE records, across a 215 s run. The scan was enabled, was answered `status 0`,
waited its 500 ms and heard nothing.

Instrumenting the HCI boundary settled it in one line — **zero packets** read
from the connector during the scan window. Not reports that failed to parse, not
reports discarded by the RSSI filter: nothing arrived, while every command still
got its Command Complete back through that same queue. An advertising report is
an LE Meta Event, gated by bit 61 of the controller's event mask, and the
specification's default mask leaves that bit clear. `HCI_Reset` restores the
default, and `Scanner::new` sent the reset and then only the scan commands. The
controller was filtering out the one event the scan exists for. Command Complete
is not maskable, which is exactly why every layer looked healthy.

`SET_EVENT_MASK` fixes it. On the same board, same assignment, same 500 ms
window:

| | Packets read | Advertisers kept |
| --- | --- | --- |
| Before | 0 | 0 |
| After | 92–131 | 43–55 |

A sweep penalty recorded here in an earlier draft was taken before that fix, and
is retracted below rather than carried forward. It is worth being exact about
what was wrong with it, because the obvious reading is wrong: the scan really was
running — `HCI_LE_Set_Scan_Enable` succeeded and the controller was holding the
antenna for its 500 ms — and only the delivery of the reports was masked. The
airtime that measurement recorded was therefore real, which is why the corrected
figure lands within 0.2 points of it rather than somewhere else entirely.

### The coexistence result

Re-measured with BLE genuinely scanning, both nodes on indices 14..=36:

| Node | BLE | Assignment acked | Sweep period |
| --- | --- | --- | --- |
| `4F:98` | off | 6900 µs | 3.299 s |
| `57:84` | **on** | **5800 µs** | 3.628 s |

Against Phase 0, where the BLE-on node acknowledged none of 32 admin frames and
completed nine sweeps to the BLE-off node's sixteen in the same 170 s:

- **It acknowledges.** First attempt, in less than six milliseconds, on the board
  that never once managed it under vendor firmware.
- **The penalty is 10.0%**, not the ~78% Phase 0 measured. The `4F:98` figure of
  3.299 s also matches the 3.298 s the pool's 5 GHz run gave a lone node, so the
  baseline is not drifting.
- The cost is almost exactly the scan window itself. Fixing the event mask
  changed the penalty by 0.2 points (9.8% to 10.0%) while taking the node from
  zero advertisers to fifty-odd per scan, so parsing, dedup and broadcast are
  nearly free next to the 500 ms of listening.

Over 27 scans the node heard a mean of 52 advertisers and a maximum of 60,
reported 90 distinct ones to the host, and dropped none. The dedup ring is
visible in the sequence: 54 new on the first scan, then 7, then 3, then zeroes.

**`REPORTS` was 64 and the maximum held in one scan was 60.** Nothing was dropped
here, but four slots of margin in an ordinary room is not margin. A denser one
would silently lose advertisers to the newest-dropped rule, and `Scanner::dropped`
is the only place that would say so — it is not on the wire and no host sees it.
`REPORTS` is now 80. The counter is still unread by every host, which is the
part that makes overflow worth avoiding outright rather than detecting. It is not
higher because `Scanner` is constructed by value, so the ring lands on the stack:
at 96 entries an `esp32c5,ble` build trips `clippy::large_stack_frames`, and the
C5 is the binding target — a C6 reaches 112. A ring bigger than that wants the
reports in a `static`, as `sniff` already keeps its sightings.

A review after the run found that `sweep` enabled its scan through the same
helper the setup commands use, which drains the controller's queue until two
reads come back empty — so it was discarding whatever advertising reports
arrived while it waited for the acknowledgement of the command that started the
scan. The fix writes the command and lets the collect loop absorb the completion
instead. The section below is what that changed, which is less than this
paragraph originally predicted.

### The drain fix changed nothing measurable, and that is the result

Re-run on the bench after the fix. The first run compared two boards
simultaneously; `4F:98` would not answer `espflash` this time (see below), so
this is the same board, `57:84`, measured twice a few minutes apart with the same
`A` assignment of indices 14..=36. Same board is the better control for the
penalty and the worse one for the room, so `4F:98` — still sweeping on its old
flash — is carried as an environment check.

| | Before the fix | After the fix |
| --- | --- | --- |
| Advertisers per scan, mean | 52.4 | 51.8 |
| Advertisers per scan, min–max | 47–60 | 45–61 |
| Distinct advertisers to the host | 90 | 91 |
| Dropped to a full ring | 0 | 0 |
| Scans | 27 | 28 |

**The prediction written here was that the re-run would report more advertisers
per sweep. It does not.** The counts are the same to within the noise of two
runs minutes apart, and the honest reading is that the drain was costing almost
nothing in this room. The worst case reasoned about — 64 ms of the window — needs
the queue to stay busy enough that two consecutive 2 ms reads never both come
back empty. At the ~50 advertisers per 500 ms this room actually produces, a
report arrives about every 10 ms, so a 4 ms gap turns up almost immediately and
the drain exits after one or two reports rather than after hundreds.

That does not make the fix wrong: the failure it removes is real, and it is
load-dependent in the direction that matters, since the denser the room the more
it takes. It does mean the counts recorded above were never depressed in any
measurable way, and the word "floor" has been removed rather than left to imply
a correction that did not happen.

The sweep penalty survives the change of method:

| | BLE off | BLE on | Penalty |
| --- | --- | --- | --- |
| First run, two boards at once | 3.299 s (`4F:98`) | 3.628 s (`57:84`) | 10.0% |
| Re-run, one board twice | 3.358 s | 3.612 s | **7.6%** |

`4F:98` held 3.042 s and then 2.990 s across the two halves of the re-run, a 1.7%
drift, so the 7.6% is `57:84`'s own and not the room moving. The assignment was
acknowledged in 5909 µs with BLE off and 6550 µs with it on — both first attempt,
both on the board that managed none of thirty-two under vendor firmware. Whether
the true figure is 7.6% or 10.0% is not settled by two runs; what both say is
that it is under ten percent and nothing like Phase 0's ~78%.

`REPORTS` was raised to 80 before this run. The maximum held in one scan was 61,
one more than the 60 that prompted the change, so the old ceiling of 64 now looks
closer than it did rather than further away.

### A board that would not flash, and a sweep that was too fast

`4F:98` refused `espflash` on every strategy offered — `usb-reset`, `no-reset`,
`no-reset-no-sync` with the chip named, a lower baud, `board-info`, `reset` and
`monitor` all failing at `Connecting...`, with nothing holding the port and the
device enumerating normally with the right serial number. It ran throughout:
heartbeating, holding an assignment, acknowledging one in 6146 µs.

**It was the computer's USB port.** Moving the same board, on the same cable, to
a different port on the host made it answer on the first attempt. Two things are
worth keeping from the hour that took, because both were wrong in the same
direction — blaming the far end:

- `espflash` reports *"Secure Download Mode is enabled on this chip"* on the
  `no-reset-no-sync` path. It is not. `esptool` on the same board says
  *"No serial data received"*, which is the truth: espflash infers the mode from
  silence. A second implementation was what distinguished them.
- A replug, a different cable, and a BOOT-strapped power-on all failed, and the
  BOOT strap demonstrably worked — the heartbeats stopped, so the chip was in the
  ROM with no application to blame. That looked conclusive for a dead peripheral
  and was not, because every one of those tests held the host port constant.

The too-fast sweep resolved with the reflash. On indices 14..=36 the old flash
held 2.99–3.04 s, where 23 dwells at 125 ms plus the 300 ms admin window is at
least 3.17 s. Reflashed with the current build the same board on the same
assignment gives 3.298 s, against the 3.299 s it gave in the first run. It was an
older flash, as suspected, and it is no longer on the board.

### Three runs, and what they agree on

| Run | Method | BLE off | BLE on | Penalty |
| --- | --- | --- | --- | --- |
| First | two boards at once | 3.299 s | 3.628 s | 10.0% |
| Re-run | one board, twice | 3.358 s | 3.612 s | 7.6% |
| Third | two boards at once | 3.298 s | 3.616 s | **9.6%** |

The two-board runs agree to within 0.4 points, and their BLE-off halves agree to
1 ms — 3.299 s and 3.298 s, on different boards, weeks of bench time apart. The
same-board run is the odd one, and it is its *BLE-off* half that is odd: 3.358 s
where every other measurement of that quantity is 3.298–3.299 s. So the penalty
is about 9.6–10.0%, and the 7.6% is best read as a baseline that drifted high
rather than as a scan that got cheaper.

Assignments were acknowledged in 5822 µs and 5798 µs, both first attempt, on
nodes holding identical 23-channel ranges. Advertisers came in at a mean of 43.8
per scan over 27 scans, min 30, max 50, none dropped, 98 distinct reaching the
host. That mean is below the 51.8 and 52.4 of the earlier runs; the room is the
same room several hours later, and nothing in the firmware changed between them.

### The stagger, with the run it asked for

This is the run the "Not measured" list has been asking for: two nodes, *equal*
assignments — both indices 14..=36 — so their sweep boundaries coincide instead
of drifting past each other. `node_count` 2 with indices 0 and 1 puts the
transmit stagger at 0 ms and 60 ms.

**Every heartbeat arrived.** 60 of 60 from `4F:98` and 53 of 53 from `57:84`,
counter spans with no gaps, where the earlier unequal-assignment run lost 3 of 65
on one node. That is the condition the stagger exists for, and nothing was lost
in it.

It is one run and it has no control: the stagger cannot be switched off from the
host, so this does not separate "the stagger prevented collisions" from "there
were no collisions to prevent". What it does retire is the possibility that
coinciding boundaries are lossy *with* the stagger in place, which is what the
earlier run left open.

## Not measured

Every checkpoint item now has a hardware answer. What is left is narrower:

- **DFS above 100 on a C5.** Unreachable rather than unmeasured, per the section
  above. DFS 52–64 is confirmed working.
- **What the transmit stagger actually prevents.** The equal-assignment run above
  is the one this asked for, and it lost no heartbeats at all. It still has no
  control, because the stagger cannot be disabled from the host; separating "it
  worked" from "there was nothing to prevent" needs a firmware build that omits
  it.
- **Which of the three coexistence measures is doing the work.** The section
  above shows the combination succeeding where the vendor firmware failed, but it
  varies none of them, so their individual contributions are unmeasured.
- **What the scan-enable drain cost in a dense room.** The re-run shows it cost
  nothing measurable in an ordinary one, which is the opposite of what was
  predicted and is not the same as showing the bug was harmless. A room dense
  enough to keep the controller's queue from going quiet for 4 ms at a time is
  what would separate them, and this bench cannot produce one.
- **Whether the advertiser count moves with the room.** Three runs give means of
  52.4, 51.8 and 43.8 per scan with identical firmware in the third, so the count
  is dominated by something the bench does not control.
- **What an initialised-but-disabled Bluetooth controller costs.** Phase 2 makes
  the scan a per-node assignment rather than a build flag, so a `ble` build
  ordinarily runs with `BleConnector::new` done and `HCI_LE_Set_Scan_Enable`
  never sent. Every measurement above had scanning *on*, so this state had never
  been on a bench. The Phase 0 failure was blamed on an initialised NimBLE stack
  keeping the radio, which is uncomfortably close to the same shape; the
  difference is that nothing here has a host stack and no scan is enabled.

  **Measured in Phase 2, and it costs nothing this bench can see**: a `ble`
  build with no assignment and a plain build differ by less than the same
  binary differs from itself between runs. So the suggestion that followed from
  this — bring the controller up on the first assignment rather than at boot —
  is not warranted. `docs/phase-2-findings.md` has the four runs.
