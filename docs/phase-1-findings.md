# Phase 1 — what the node firmware actually does

Measured on 2026-09-06 with one ESP32-C6 running `firmware/bridge`
(`98:A3:16:8E:9D:24`, USB) and one ESP32-C6 running `firmware/node`
(`A0:F2:62:87:00:08`, `esp32c6` feature, BLE not compiled in). Both plaintext,
control channel 6. No C5 was attached, so nothing here is a 5 GHz or DFS
result.

Access point addresses and names are left out deliberately: these were real
captures of a real neighbourhood, and a BSSID is exactly what a geolocation
database is built from. Counts and channels are the whole of what was being
checked, and they carry none of that.

## Sniffing and ESP-NOW do coexist

The design's largest open risk — that promiscuous mode would starve the ESP-NOW
receive path, and that the whole `sniffer()`/`esp_now()` shared-borrow shape
would have to be abandoned for `scan_async` — did not materialise. A node
holding both for the life of the program reported observations and acknowledged
assignments in the same sweep, repeatedly, across three sessions.

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
assignment, and the re-issue after a reboot — measured 5908 µs and 6141 µs
heartbeat-to-callback on the bridge's own clock, against a 300 ms admin window.

**Six identical assignments went out in one 9 ms burst at connect.** This is
host behaviour, not node behaviour, and it is benign: a bridge left powered
buffers heartbeats, so `run` attaching to one that has been up for half an hour
processes a backlog of them before any acknowledgement can return, and emits an
assignment per heartbeat. All six carried epoch 1, the node adopted the first
and discarded five on the `!=` rule, and all six were acknowledged — which is
also an incidental confirmation of the firmware's central claim, since a stock
node is only listening for 300 ms per sweep and could not have heard six frames
sent inside 9 ms. Their `latency_us` reads 7.4–9.6 s because the paired
heartbeats genuinely were that old; the metric is correct and the backlog is
what it is measuring. It is bounded by the bridge's ring and does not recur in
steady state.

## Sweep period, with the caveat that makes it not a fair fight

Phase 0 predicted about 3.6 s per sweep for a vendor node holding one channel,
against 6.7 s measured for eleven. This firmware beat every 1.7 s across
channels 1–11 and every ~430 ms on a single channel.

That is not a like-for-like win. The vendor figure carries a BLE scan this
build does not compile in, and Phase 0 is explicit that the overhead is fixed
rather than proportional. What the numbers do establish is that the 300 ms
admin window is being honoured and is the dominant cost at a narrow
assignment — 125 ms of dwell against 300 ms of listening — which is the shape
the design intends and the reason a one-channel node is the case the BLE rate
limit had to be safe for.

## Channel membership holds

The check `docs/phase-0-findings.md:116-118` recommends over heartbeat period.
Every observation named a channel inside the range the node had been assigned,
in both sessions and with nothing outside:

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

`sniff` and `run` both reported `garbled 1` in every session. It is always the
same thing, and it is always immediate:

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

## Not measured

Three checkpoint items had no hardware to run on and are still open:

- **5 GHz and DFS on a C5**, including the claim that a listening node
  transmits no probe requests on channels 52–144. This is the strongest reason
  the firmware sniffs rather than scans and it is entirely unverified.
- **A two-node split**, where the plan is cut in half and each node stays inside
  its own range. Everything above is one node, so `node_index`/`node_count` have
  only ever been 0 and 1, and the transmit stagger has never been exercised.
- **BLE**, which was not compiled in. Whether the three coexistence measures in
  `firmware/node/src/ble.rs` are enough to keep a node acknowledging is the
  question Phase 2 is for, and Phase 0's table is still the only data on it.
