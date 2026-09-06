# Phase 1 — what the node firmware actually does

Measured on 2026-09-06. The bridge throughout is an ESP32-C6 running
`firmware/bridge` (`98:A3:16:8E:9D:24`, USB). Everything up to "Every connection
begins with one undecodable frame" used one ESP32-C6 node
(`A0:F2:62:87:00:08`, `esp32c6` feature); the 5 GHz section that follows used
one ESP32-C5 (`38:44:BE:1F:57:84`, `esp32c5` feature) in its place. Never both
at once, so nothing here is a two-node result. BLE was not compiled into either.
Plaintext, control channel 6.

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

## The C5 does 5 GHz, and refuses ten channels of it

Added 2026-09-06 with an ESP32-C5 (`38:44:BE:1F:57:84`, the BLE-on node from
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
  on this `esp-radio`.** Whether that is a regulatory-domain default that can be
  configured or a hard limit of the driver is not established here, and it is
  worth finding out before Phase 2 reshapes assignments around a channel mask.
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

### Probe requests: argued, not captured

The claim that a listening node transmits nothing on a DFS channel is *not*
verified by a monitor-mode capture here. It is verified by construction: every
transmit in the sweep — `report` and the heartbeat both — is gated on
`on_control`, the return of the park back to channel 6, and there is no other
transmit path in the loop. The node cannot send on a dwell channel because no
code does. What a capture would still add is whether the driver itself emits
anything while parked, which this does not answer.

## Not measured

Two checkpoint items had no hardware to run on and are still open, and one is
now half-answered:

- **DFS above 100 on a C5.** Unreachable rather than unmeasured, per the section
  above. DFS 52–64 is confirmed working.
- **A two-node split**, where the plan is cut in half and each node stays inside
  its own range. Everything above is one node, so `node_index`/`node_count` have
  only ever been 0 and 1, and the transmit stagger has never been exercised.
- **BLE**, which was not compiled in. Whether the three coexistence measures —
  the two in `firmware/node/src/ble.rs` and the rate limit that is
  `BLE_INTERVAL_MS` in `main.rs` — are enough to keep a node acknowledging is the
  question Phase 2 is for, and Phase 0's table is still the only data on it.
