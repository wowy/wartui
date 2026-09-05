# Phase 0 — what the hardware actually does

Measured on 2026-09-05 with a JCMK C5 wardriver as CORE (`10:BD:A3:D7:92:E8`),
one ESP32-C5 as NODE (`38:44:BE:1F:59:50`), and an ESP32-C6 running
`tools/espnow-sniffer` on channel 6. Encryption off on both devices.

## The mesh is plaintext, and easily audible

Every node frame is broadcast to `FF:FF:FF:FF:FF:FF` at about −40 dBm on the
bench. The ESP-NOW receive callback and promiscuous capture agree frame for
frame, which is what a bridge needs.

This settles the project's largest open risk. Nodes drop to 2 dBm while
wardriving (`RadioTuning.cpp:15-23`) and it was not obvious a dongle would hear
them at all.

## The wire format matches, byte for byte

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
20..39, byte for byte what `wartui-proto`'s planner produces — and each
assignment was again retried 31 times with no acknowledgement.

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

So the vendor firmware's fleet coordination does not function here. A wartui
bridge that retries an assignment until the radio confirms delivery is not a
refinement on the existing behaviour; it is the difference between the feature
working and not working.

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

### Still not known

Why the node does not acknowledge. Candidates worth ruling out: BLE
coexistence taking the radio during the admin window — plausible given the BLE
scan accounts for roughly half of each sweep — an asynchronous Wi-Fi scan still
owning it, or `setFixedChannel` leaving the interface in promiscuous mode on
its error path (`WiFiOps.cpp:608-612` returns without restoring it).

This matters for wartui only if the cause also blocks a bridge from reaching
the nodes. Phase 4 will find out directly, and its `SendResult` will say so
rather than leaving it to be inferred.

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
stable topology keeps the 40-channel default indefinitely.
