# Symptom catalogue

For each symptom the survey can flag, this table gives what it usually means and where to read
before naming a cause. The "usual meaning" column is the opening hypothesis. Only the code and the
capture confirm it. Paths are relative to the repo root. A `//!` at the top of each file holds the
reasoning.

## Loss

| Symptom | Usual meaning | Read |
|---|---|---|
| Heartbeat `beat` gaps on one node far above the rest | That node's air path is weak or busy: distance, placement in the car, or a radio that is off channel when it beats. Compare its link RSSI and job with the others. | `crates/wartui-core/src/analyze.rs` |
| Heartbeat misses that rise as one node's link RSSI falls, while its siblings at the same RSSI miss far fewer | Something specific to that node: its board, antenna or mount, or its slot. Read the survey's final share per node. The node dealt channel 6, the control channel, is a confound. A raw/stored heartbeat count that matches means the frames never reached the bridge. They were lost on the air, or never sent: `heartbeat()` discards the result of `radio::broadcast`. | `firmware/node/src/main.rs` (`heartbeat`, last line), `firmware/node/src/radio.rs` `broadcast` |
| `miss spacing` peaks at 2 or more beats rather than at 1 (survey flags "misses heartbeats periodically") | The loss is not independent. Something recurring every N beats lines up with the heartbeat. Candidates: the beat deadline drifting against the sweep, or a periodic transmitter near the bridge or node. No cause is established. Compare the period with the node's sweep length and its beat deadline, and check whether other nodes in the same capture share it. | `firmware/node/src/main.rs` (`next_beat` / `next_beat_after`, the dwell loop) |
| Heartbeat gaps only on sweeping nodes, none on the BLE node | Hypothesis: a beat sent just after a node parks back on the control channel is lost. Heartbeats are broadcast with no retry. | `firmware/node/src/main.rs` (dwell loop), `firmware/node/src/radio.rs` `park` |
| Rows in `batch_gap` | Unicast sighting batches the bridge never passed up. The MAC layer retried and gave up, or the bridge ring evicted them. | `docs/batch-loss-findings.md`, `crates/wartui-proto/src/outbox.rs` |
| Bridge `dropped_tx` rising | The bridge's ring to the host overflowed because the host was not reading fast enough. | `crates/wartui-proto/src/outbox.rs` `//!`, `docs/usb-boundary-findings.md` |
| `~lost on USB` > 0 | Frames lost between the bridge queue and the host read. It is approximate to ±24 frames. | `crates/wartui-core/src/analyze.rs` `//!`, `docs/usb-boundary-findings.md` |
| `store_dropped` > 0 | The store queue was full and rows were dropped by design rather than blocking the engine. Check the commit peak and the SD card. | `crates/wartui-core/src/store.rs` `//!`, `docs/store-io-findings.md` |
| Ring refusals (`wifi_dropped`/`ble_dropped` deltas) | Ring pressure on the node: a sighting was turned away once per dwell. Usually reported on a later dwell, so not loss. | `crates/wartui-proto/src/outbox.rs`, `crates/wartui-core/src/analyze.rs` `//!` |
| `duplicate_batches` | Retransmits the dedup caught. The batch was stored once, so this is not loss. | `crates/wartui-proto/src/dedup.rs` |

## Admin and plan

| Symptom | Usual meaning | Read |
|---|---|---|
| Assignment outcome other than `acked` | No MAC-layer ack. Only an ack is believed; an enqueue is not. | `crates/wartui-core/src/engine/admin.rs`, `crates/wartui-core/src/record.rs` `AdminOutcome` (`unacked` is usually BLE coexistence on the node) |
| Heartbeat epoch differs from the last acked epoch long after the ack | The node acked the frame but did not adopt it, or a restarted node lost its epoch. The host should re-send on that heartbeat's window. | `crates/wartui-core/src/engine/node.rs` `NodeState::adopted`, `snapshot.rs` `Counters::admin_unadopted` |
| Epoch numbers with no `assignment` row | Epochs allocated by a replan and superseded before transmission. This is normal at startup while nodes join. It is suspicious mid-drive. | `crates/wartui-core/src/engine/replan.rs` |
| Many assignment rows per hour mid-drive | Fleet membership churn: nodes timing out and rejoining. Pair it with heartbeat silences. | `crates/wartui-core/src/engine/replan.rs`, `topology_timeout` |
| `admin_windows_missed`, or host lag ≥ 100 ms | The host was too far behind the air to answer a heartbeat's admin window. | `crates/wartui-core/src/engine/lag.rs` `BEHIND_THE_AIR` |
| Replayed heartbeats (`live=0`) | The bridge backlog drained after the host reconnected. These are never treated as admin windows. | `crates/wartui-core/src/engine/lag.rs` `note_arrival` / `air_is_live` |
| `preferred_ble` set but no node holds the BLE job | The remembered Bluetooth node was not in this fleet, so nobody scanned Bluetooth. This is operator configuration, not a bug. | `crates/wartui-core/src/engine/event.rs` `Command::RememberBle`, `crates/wartui/README.md` |
| BLE job assigned but zero `ble` rows | The node scanned and stored nothing. Check `firmware/node/src/ble.rs` and the engine's `Frame::Sightings` arm. | `firmware/node/src/ble.rs`, `crates/wartui-core/src/engine/rx.rs` |

## Bridge and link

| Symptom | Usual meaning | Read |
|---|---|---|
| `uptime_ms` fell | The bridge rebooted. A software reset does not re-enumerate USB, so this is the only evidence. The likely trigger is the stall watchdog. | `crates/wartui-proto/src/stall.rs` `StallWatch`, `crates/wartui-bridge/src/serial.rs` `Ready` arm, `docs/phase-3-findings.md` |
| Long gaps between `bridge_status` replies | The link was down, or the host was not polling. Match against the log for reconnects. | `crates/wartui-bridge/src/serial.rs` |
| `peer_count` = nodes + 1 | Expected. The bridge registers a broadcast peer at init, which is why it reads 1 before any node joins. | `firmware/bridge/src/main.rs` (broadcast peer setup) |
| `peer_count` above nodes + 1 | Peers not yet aged out (they are removed one `topology_timeout` after the last heartbeat), or nodes that never sent a heartbeat. | `crates/wartui-core/src/engine/mod.rs` |
| `rx_count` flat while `uptime_ms` keeps rising | The bridge was up but heard nothing over the air. At the start of a capture this means the fleet was powered after the host. Mid-drive, together with every node restarting, it means the nodes lost power. | `bridge_status` rows; the survey prints the longest such stretch |
| `garbled` | COBS or link frames that did not decode, usually resynchronisation after a truncated USB write. | `crates/wartui-proto/src/link.rs`, `crates/wartui-proto/src/outbox.rs` |
| `undecodable` | Counts two things: a frame without our magic that `air::foreign` does not recognise (most often a stranger's ESP-NOW device), and a frame with our magic and version whose body would not parse. Only the second is our bug. A short burst at one place is usually the first. The capture cannot tell them apart without raw frames. | `crates/wartui-core/src/engine/rx.rs` (both increments), `crates/wartui-proto/src/air.rs` |
| `incompatible` | Frames carrying our magic with an unknown wire version: a node on another build. The fix is to reflash the fleet together. | `crates/wartui-proto/src/air.rs` `WIRE_VERSION` |
| `foreign_fleet` / `foreign_admin` | Somebody else's wartui-like fleet nearby. It is reported, never accommodated. | `crates/wartui-proto/src/air.rs` `air::foreign` |

## Sightings and decode

| Symptom | Usual meaning | Read |
|---|---|---|
| A channel outside the scan table (0, odd 5 GHz numbers) | The channel comes from the frame's own element (DS Parameter Set or HT operation) when present, otherwise from the parked channel. A garbled or vendor-odd element yields a nonsense channel. | `crates/wartui-proto/src/beacon.rs` (channel parsing) |
| 5 GHz rows on a channel never assigned to the node | Usually an adjacent or HT40-secondary beacon heard while parked nearby. It is a bug if a node repeatedly reports a whole band it was not given; check `park`. | `firmware/node/src/radio.rs` `park`, `crates/wartui-proto/src/plan.rs` |
| No 5 GHz rows at all | The planner found 5 GHz unreachable (the only 5G radio held the BLE job), or the regulatory domain is wrong. | `crates/wartui-proto/src/plan.rs` `Plan::unreachable`; AGENTS.md (the `US` domain rule) |
| Zero BSSID, or the multicast bit set on a BSSID | Parsing took the wrong address field, or the frame was not a beacon or probe response. | `crates/wartui-proto/src/beacon.rs`, `firmware/node/src/sniff.rs` |
| Security `[UNDEFINED]` | No RSN or WPA element, and the privacy bit is ambiguous. Check which frame types produce it. | `crates/wartui-proto/src/beacon.rs` |
| Unknown `kind` | A kind this build does not know. The export skips it. | `crates/wartui-core/src/export.rs`, `crates/wartui-core/src/store.rs` (kind mapping) |
| A node heartbeats but stores no sighting for minutes | The node is parked on the control channel with no assignment, or its channel change did not stick, or the area really was empty. Compare with other nodes in the same window. If they saw networks, it is the node. | `firmware/node/src/radio.rs` `park`, `crates/wartui-core/src/engine/node.rs` |

## Host, GPS and clock

| Symptom | Usual meaning | Read |
|---|---|---|
| Gaps over 15 s between `host_status` rows | The host process stalled, the machine slept, or power dipped. | `crates/wartui-core/src/runtime.rs` |
| Pi under-voltage or throttling | Supply sag. Expect the bridge or USB to misbehave in the same minutes. | `crates/wartui-core/src/health.rs` |
| `pos_source = none` stretches | GPS lost its fix, or the receiver was not found. Positions resolve per record through `PositionChain`. Never suggest `--lat/--lon` as a fallback. | `crates/wartui-core/src/position.rs`, `crates/wartui-core/src/gps.rs`, `crates/wartui-core/src/discover.rs` |
| Fix age above `gps_max_age_ms` | The chain handed out a stale fix, which would be a bug. | `crates/wartui-core/src/position.rs` |
| Position jumps above 250 km/h | NMEA glitches or a cold-start fix. The exported rows carry the bad position. | `crates/wartui-core/src/nmea.rs`, `crates/wartui-core/src/gps.rs` |
| `rx_at` steps backwards | The host wall clock was set mid-run by NTP or GPS time. Analysis walks rows by `id` for this reason. | `crates/wartui-core/src/analyze.rs` `//!` |
| Every node restarts within seconds while the bridge stays up | A shared supply to the nodes was cut, for example engine off or a socket. A panic on all boards at once is very unlikely. The heartbeat carries no reset reason, so the capture cannot prove it. | `crates/wartui-core/src/engine/rx.rs` (reboot detection), `crates/wartui-proto/src/air.rs` `HeartbeatMsg` |
| `battery_ma` negative throughout, voltage falling | The host ran on its own battery and was not charging. This affects range, not data. | `crates/wartui-core/src/health.rs` |
| `upload.result` is `unfollowed` | The host stopped waiting before the site said done or failed. Whether it imported is unknown; check the job on the site. Never re-upload without asking. | `crates/wartui/src/upload.rs` |
| An uploaded row count differs from what `wartui export` gives today | The build that uploaded exports differently from this one, for example one that keeps zero or group BSSIDs. Find the rows that differ before blaming the site. | `crates/wartui-core/src/export.rs` `//!` |
| `heartbeat.admin_*` all 0 or NULL while assignments went out | The columns are declared but the heartbeat insert does not write them. This is a host finding, so do not use them as evidence. | `crates/wartui-core/src/store.rs` (heartbeat insert) |
| `ended_at` NULL | The host died or was killed before shutdown. Check the log tail. | `crates/wartui-core/src/store.rs` |
