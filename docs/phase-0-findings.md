# Phase 0 — what the hardware actually does

Measured on 2026-09-05 with a JCMK C5 wardriver as CORE (`92:E8`),
one ESP32-C5 as NODE (`59:50`), and an ESP32-C6 running
`tools/espnow-sniffer` on channel 6. Encryption off on both devices.

## The mesh is plaintext, and easily audible

Every node frame is broadcast to `FF:FF:FF:FF:FF:FF` at about −40 dBm on the
bench. The ESP-NOW receive callback and promiscuous capture agree frame for
frame, which is what a bridge needs.

This settles the project's largest open risk. Nodes drop to 2 dBm while
wardriving (`RadioTuning.cpp:15-23`) and it was not obvious a dongle would hear
them at all.

## The wire format matches, byte for byte

> **Superseded, 2026-09-10.** True of the fleet measured here and of wartui at
> the time. It is no longer true of wartui in either direction: every frame now
> carries wartui's own magic, and the vendor vectors and the C++ generator that
> produced them have been deleted along with `tools/golden`. The reason is in
> `crates/wartui-proto/src/air.rs` and comes straight out of this document's own
> measurements — two fleets that share a format share one conversation. What
> survives here is the record of what the vendor firmware puts on the air, which
> is what `air::foreign` recognises in order to report it.

129 captured frames decode and re-encode identically, and are checked in as
golden vectors in `crates/wartui-proto/tests/golden_vectors.txt`. They confirm
212-byte text frames and 10-byte admin frames, `ENOW` magic, little-endian
counter at offset 5 and length at 9, payload at 11, and zero padding to the
end.

Observations cover channels 1, 6, 11, 36, 48, 56 and 149, four of the
firmware's auth tokens, and twelve hidden access points with empty SSIDs.

## Channel assignments are not being delivered

**This is the significant finding.** The core's `MSG_ADMIN` is unicast, and the
node never acknowledges it.

Within a single capture:

| Frame type | Frames | Distinct 802.11 sequence numbers | Retry bit set |
| --- | --- | --- | --- |
| `MSG_HEARTBEAT` (broadcast) | 28 | 28 | 0 |
| `MSG_TEXT` (broadcast) | 143 | 143 | 0 |
| `MSG_ADMIN` (unicast) | 32 | **1** | **31** |

Broadcasts each carry their own sequence number and are never retried, which is
correct — nothing acknowledges a broadcast — and doubles as proof the sequence
extraction works. The 32 admin frames are one transmission the radio kept
resending because no acknowledgement came back. Reproduced across three
separate captures.

Acknowledgements were then captured directly: of 9505 seen on the channel,
**none** named the core. So two independent receivers agree — the core's own
radio, which retried, and the sniffer.

Meanwhile the core clears its dirty flag from the `esp_now_send` return value
(`WiFiOps.cpp:679`), which reports only that the frame was queued, and moves on
believing the node was assigned. The node's local `assignment_version` stays at
0 and it keeps scanning all 40 channels.

With one node this is invisible, because a lone node is assigned the whole
table anyway. With two or more it would mean the channel split silently never
takes effect.

### What this means for wartui

The design already anticipated it — divergence 3, "clear ADMIN-dirty on
MAC-ACK, not enqueue-OK" — so no change is needed, but it is now evidence
rather than caution. `SendResult` carrying the transmit-callback status is the
feature that makes assignment delivery observable at all.

Two things follow:

- An unacknowledged assignment must be retried on the node's next heartbeat.
  Heartbeats arrive every few seconds, so a retry costs one sweep.
- `MSG_ADMIN` carries no destination field, and the node's handler
  (`WiFiOps.cpp:1193`) does not check who a frame was addressed to. Broadcasting
  it would therefore be delivered reliably, but every node would adopt the same
  assignment — so it is only ever correct for a single-node fleet, and is not a
  general fix.

### Confirmed with two nodes: the split never happens

Running a second node made the consequence visible. The core computed and sent
the split correctly — version 3, index 0 of 2 on indices 0..19, index 1 of 2 on
20..39, byte for byte what `wartui-proto`'s planner produced at the time — and
each assignment was again retried 31 times with no acknowledgement. (That last
comparison stopped being possible in Phase 2, when the assignment became
wartui's own frame; the planner is checked against properties now, in
`crates/wartui-proto/tests/planner.rs`.)

Neither node adopted it. The decisive evidence is which channels each node
reported networks on afterwards, since a node can only report an access point
on a channel it actually scanned:

| Node | Assigned indices | Assigned channels | Reported on | Outside its range |
| --- | --- | --- | --- | --- |
| `…57:84` | 20..39 | 60–177 | channel 6 ×16, 149 ×2 | **16 of 18** |
| `…59:50` | 0..19 | 1–56 | channel 6 ×12, 48 ×2 | 0 of 14 |

Node `…57:84` was assigned the 5 GHz upper half and spent its time on channel
6, which is index 5. Both nodes are still sweeping the whole table, duplicating
each other's work — precisely what the channel assignment exists to prevent.

So the vendor firmware's fleet coordination did not function on this fleet. A
wartui bridge that retries an assignment until the radio confirms delivery is
not a refinement on the existing behaviour; it is the difference between the
feature working and not working.

Every node in this capture had BLE enabled, which the next section shows is the
reason nothing was acknowledged. The failure is real and it is the default
configuration, but it is conditional rather than absolute.

### A correction to the Phase 4 milestone

The plan proposed confirming an assignment by watching a node's heartbeat
period collapse when narrowed to one channel. That signal is weaker than it
looked. Measured here, a 40-channel sweep takes about 6.7 s, of which only
3.2 s is channel dwell (40 × `CHANNEL_TIMER` 80 ms); the rest is the per-sweep
BLE scan plus the 300 ms admin window, and that overhead is fixed. Halving the
channel count moved the period from 6.7 s to — nothing, because the assignment
never arrived, but even had it arrived the expectation was 5.1 s, not 3.4 s.

**Use channel membership instead.** Check that every Wi-Fi observation a node
reports names a channel inside its assigned range. It is unambiguous, needs no
timing, and it is what actually caught this. Heartbeat period remains a useful
secondary signal at a one-channel assignment, where the predicted period is
about 3.6 s against 6.7 s.

### Why the node does not acknowledge: BLE coexistence

A second two-node capture settled it, and it was a controlled experiment rather
than an inference. Both nodes ran the `feat/node-interference-mitigation`
firmware, the core ran `main`, and the only difference between them was that
one node had `ble_enabled` off.

| Node | BLE | ADMIN frames the core sent it | Acknowledged |
| --- | --- | --- | --- |
| `…59:50` | off | 2, each transmitted once, no retry | **2 of 2** |
| `…57:84` | on | 32, one sequence number, retry bit on 31 | **0 of 32** |

Both assignments were sent immediately after that node's own heartbeat, so both
landed inside the 300 ms admin window; the frames were 1 dB apart at the
sniffer. An 802.11 acknowledgement is generated by the receiver's MAC hardware,
not by firmware, so its absence means the radio was not listening on channel 6
at that instant. NimBLE shares the single 2.4 GHz antenna, and the admin window
is exactly when the node is neither scanning nor transmitting — the window the
BLE controller is free to take.

BLE is also what makes the window come around so rarely. In the same 170 s the
BLE-off node completed 16 sweeps and the BLE-on node 9, so the per-sweep BLE
scan costs roughly as much as every channel dwell combined.

The earlier reading, that assignments are simply never delivered, was measured
on a fleet where BLE was on everywhere. The accurate statement is narrower and
more useful: **unicast ADMIN is reliable to a node with BLE disabled and
essentially never lands on a node with BLE enabled.**

Adoption follows delivery exactly. After its acknowledged assignment to indices
0..19, `…59:50` reported channels 6, 6 and 56 — all inside 1–56. After its
unacknowledged assignment to 20..39, `…57:84` still reported channel 6, which
is index 5.

Three consequences for wartui:

- **Clearing on the acknowledgement is now load-bearing, not prudent.** Clearing
  the dirty flag on the transmit callback rather than the `esp_now_send` return
  value is the whole difference between retrying into the next admin window and
  believing a lie.
- **Retry across windows, not within one.** The radio's own 31 retries all fell
  inside a single window and all failed together. The useful retry is the next
  heartbeat, which is what the dirty flag already gives us.
- **Surface it.** A node that has not acknowledged an assignment after several
  heartbeats should read, in the fleet table, as *BLE coexistence is likely
  blocking this*, with the fix — turn BLE off in that node's web UI — named.
  wartui cannot fix it from the air; it can stop it being a mystery.

Not ruled out, and not needed for the above: whether an asynchronous Wi-Fi scan
also holds the radio, and whether `setFixedChannel` leaves the interface
promiscuous on its error path (`WiFiOps.cpp:608-612` returns without restoring
it).

## Node-side deduplication is as aggressive as expected

A node reports a BSSID once and then suppresses it until 200 further unique
MACs push it out of the ring (`WiFiOps.cpp:1699`, `configs.h:158`), and nothing
clears that ring at runtime — `clearMacHistory()` is defined at
`WiFiOps.cpp:1882` and never called. A capture taken twelve minutes into a run
contained only BLE sightings, whose addresses rotate; power-cycling the node
produced 44 Wi-Fi records in its first sweeps. Observation rates are tens per
minute, not a firehose.

## Heartbeat counters restart at 1 after a reboot

Confirmed directly: 147–151 before a power cycle, 1–6 after. This is the signal
behind divergence 5, which lets the host notice a node has rebooted and re-send
its assignment. The vendor core does not do this, so a rebooted node under a
stable topology keeps the 40-channel default indefinitely. The counter itself
carried across into wartui's own heartbeat unchanged, so the divergence and its
reasoning outlived the frame they were measured in.
