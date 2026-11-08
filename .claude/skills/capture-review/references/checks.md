# Symptom catalog

For each symptom the survey can flag, this table gives what it usually means and where to read
before naming a cause. The "usual meaning" column is the opening hypothesis. Only the code and the
capture confirm it. Paths are relative to the repo root. A `//!` at the top of each file holds the
reasoning.

## Loss

| Symptom | Usual meaning | Read |
|---|---|---|
| A node's heartbeat interval often over twice its median (survey prints `~N gaps over twice its median interval`) | Inferred from timing. A heartbeat crosses the node bus with resends, so a gap points at the node first: a sweep running long, an update in progress, or a stall. A node holding the whole pool alone sweeps for about 5 s, so its cadence is longer than 5 s. | `firmware/node/src/main.rs` (`next_beat`), `crates/wartui-proto/src/node.rs` `ASSIGNED_BEAT_MS` |
| Rows in `batch_gap` | Sighting batches the P4 acknowledged that the host never read: lost in the P4's queue or on USB. A node resends an unacknowledged batch under the same `seq`, so loss on the node bus never shows here. | `crates/wartui-core/src/engine/rx.rs` `note_batch_seq`, `crates/wartui-proto/src/host_queue.rs`, `docs/batch-loss-findings.md` |
| Bridge `dropped_tx` rising | A queue on the way to the host overflowed: `host-*` because the host was not reading fast enough, `radio-up` because the link to the P4 was. The column sums both chips, so a C6 restart lowers only part of it; the survey's `dropped` adds the queues' own drops. | `crates/wartui-proto/src/host_queue.rs` `//!`, `docs/usb-boundary-findings.md` |
| `~lost on USB` > 0 | Frames lost between the bridge queue and the host read. It is approximate to ±24 frames. | `crates/wartui-core/src/analyze.rs` `//!`, `docs/usb-boundary-findings.md` |
| `store_dropped` > 0 | The store queue was full and rows were dropped by design rather than blocking the engine. Check the commit peak and the SD card. | `crates/wartui-core/src/store.rs` `//!`, `docs/store-io-findings.md` |
| Pending-buffer refusals (`wifi_refused`/`ble_refused` deltas) | Pending-buffer pressure on the node: a sighting was turned away once per dwell or scan. Usually reported on a later one, so not loss. | `crates/wartui-proto/src/pending.rs`, `crates/wartui-core/src/analyze.rs` `//!` |
| `duplicate_batches` | The node bus resending a batch whose ack the node did not see, caught by the engine. The batch was stored once, so this is not loss. | `crates/wartui-core/src/engine/rx.rs` `DUPLICATE_BATCH_WINDOW_US` |

## Admin and plan

| Symptom | Usual meaning | Read |
|---|---|---|
| Assignment outcome other than `acked` | The bridge answered `Undelivered` (no node in the slot, its queue full, or no ack after every try), or never answered. Only a delivery is believed; an enqueue is not. | `crates/wartui-core/src/engine/admin.rs`, `crates/wartui-core/src/record.rs` `AdminOutcome`, `crates/wartui-proto/src/link/mod.rs` `Undelivered` |
| Heartbeat epoch differs from the last acked epoch long after the ack | The node took delivery but did not adopt it, or a restarted node lost its epoch. The host re-sends in answer to that heartbeat. | `crates/wartui-core/src/engine/node.rs` `NodeState::adopted`, `crates/wartui-core/src/engine/snapshot.rs` `Counters::admin_unadopted` |
| Epoch numbers with no `assignment` row | Epochs allocated by a replan and superseded before transmission. This is normal at startup while nodes join. It is suspicious mid-drive. | `crates/wartui-core/src/engine/replan.rs` |
| Many assignment rows per hour mid-drive | Fleet membership churn: nodes timing out and rejoining. Pair it with heartbeat silences. | `crates/wartui-core/src/engine/replan.rs` (`replan`), and `topology_timeout` in `crates/wartui-core/src/engine/mod.rs` (`is_alive`) |
| Host lag ≥ 100 ms | The host read the link behind the bridge. Heartbeats read then are history: their epochs are not believed, and a node whose epoch no live heartbeat has reported waits for one before it is sent a share. | `crates/wartui-core/src/engine/lag.rs` `BEHIND_US`, `crates/wartui-core/src/engine/admin.rs` `send_admin` |
| Replayed heartbeats (`live=0`) | The host drained the bridge's queue late, as after a stall. Their epochs are never believed. | `crates/wartui-core/src/engine/lag.rs` `note_arrival` / `reading_live` |
| A node heartbeating from more than one slot (survey flags "heartbeated from N slots") | The node was moved to another slot, or another node's heartbeat came through its slot. Its assignment follows its latest slot, and a node whose slot another took shows `no slot`. | `crates/wartui-core/src/engine/rx.rs` `take_slot`, `docs/node-bus.md` |
| `capture.node_board` is a carrier (survey prints `board carrier-…`) | Slots are stored by bus name, and the operator and the bridge's log lines named the silk. Read a slot through the board: bus A is `N1` up and bus B follows, so `B1` is `N4` on the 6, `N6` on the 10 and `N8` on the 14. | `crates/wartui-proto/src/bus/node_board.rs` `NodeBoard::label`, `docs/node-bus.md` § "On the carrier board" |
| `preferred_ble` set but no node holds the BLE job | The remembered Bluetooth node was not in this fleet, so nobody scanned Bluetooth. This is operator configuration, not a bug. | `crates/wartui-core/src/engine/event.rs` `Command::RememberBle`, `crates/wartui/README.md` |
| BLE job assigned but zero `ble` rows | The node scanned and stored nothing. Check `firmware/node/src/ble.rs` and the engine's `Frame::Sightings` arm. | `firmware/node/src/ble.rs`, `crates/wartui-core/src/engine/rx.rs` |

## Bridge and link

| Symptom | Usual meaning | Read |
|---|---|---|
| `uptime_ms` fell | The bridge's C6 restarted. `Ready.uptime_ms` is the C6's and its restart does not re-enumerate the P4's USB, so this is the only evidence. The C6's reset cause says why. | `crates/wartui-bridge/src/serial.rs` `Ready` arm, `crates/wartui-proto/src/split.rs` |
| Long gaps between `bridge_status` replies | The link was down, or the host was not polling. Match against the log for reconnects. | `crates/wartui-bridge/src/serial.rs` |
| `the radio has stopped answering` in the log (survey flags it) | The C6 sent no packet for 12 s after a host's poll, as when it hangs holding RTS. Radio traffic stops while `bridge_status` goes on answering with its last counters, so match `rx_count` going flat. Ends when a packet arrives; `wartui reset` restarts the bridge. | `crates/wartui-proto/src/split.rs` `RADIO_SILENT_MS`, `firmware/p4-bridge/README.md` |
| `rx_count` flat while `uptime_ms` keeps rising | The bridge was up but forwarded no node record. At the start of a capture this means the fleet was powered after the host. Mid-drive, together with every node restarting, it means the nodes lost power. | `bridge_status` rows; the survey prints the longest such stretch |
| Bridge `rx_oversize` rising (survey flags it) | The P4 dropped a node record over 1024 bytes. No node of this build sends one, so it points at a node bug or corruption on the bus. | `crates/wartui-proto/src/link/mod.rs` `MAX_NODE_PAYLOAD`, `firmware/p4-bridge/src/bus.rs` |
| A bridge queue dropped during the capture | That queue filled and evicted or refused entries. `host-bulk` and `host-priority` mean the host fell behind; `radio-up` and `radio-down` mean the UART link to the C6 did; `node-down` means records for the nodes outran the node bus. | `crates/wartui-proto/src/host_queue.rs`, `crates/wartui-proto/src/split.rs`; sizes in `firmware/p4-radio/src/config.rs` (`WARTUI_RADIO_*`), `firmware/p4-bridge/src/config.rs` (`WARTUI_P4_*`) |
| A high `peak` sightings/s in `wartui analyze`, in the same capture as bridge `dropped_tx` rising or a `host-bulk` queue peak near its capacity | The burst outran the host's read of the link. The one-second peak is the burst the bridge's queue has to absorb; the one-minute peak is the load the host has to keep up with. A per-node `peak` line that dominates names the node behind it. | `crates/wartui-core/src/analyze.rs` `peaks`, `crates/wartui-proto/src/host_queue.rs` `//!`, `docs/usb-boundary-findings.md`; typical peaks in `docs/arrival-rate-findings.md` |
| A bridge queue's peak at 90% or more of its capacity | Near full. The peak is since the bridge booted, so it can predate the capture. The compile-time `WARTUI_*` size for that queue is the lever. | The same `config.rs` files; `bridge_queue` rows |
| A byte queue's peak far below its capacity on every drive | RAM the P4 or C6 could spend elsewhere. Right-size it from several drives' peaks, not one. | The same `config.rs` files; `bridge_queue` rows |
| Link `crc_fail`, `bad_magic`, `seq_gap` or `uart_errors`, or their `radio_*` counterparts | Corrupt or skipped packets between the P4 and the C6: wiring, baud rate or framing. `uart_errors` are line errors at the receiver. Plain counters are C6→P4 as the P4 saw them; `radio_*` are P4→C6 as the C6 saw them. One `seq_gap` per C6 restart is expected, so the survey warns only on those past the capture's restarts; its `radio link` line prints the raw sum. | `crates/wartui-proto/src/radio_link/`, `crates/wartui-proto/src/split.rs`, `firmware/p4-bridge/src/link.rs`, `firmware/p4-radio/src/link.rs` |
| `host_uart_errors` > 0 | P4 host-link reads that saw one or more line errors on the UART from the host: FIFO overruns, framing, parity or glitches. One read counts once however many it saw, and each is host bytes lost or corrupted on the way in. The P4 overruns while it erases its own flash with interrupts off. A `flash-bridge` update erases, and rewinds over the loss; so does one fan duty change in 1024, which nothing rewinds. In a capture, suspect the cable, the CH343 or a P4 stall. | `firmware/p4-bridge/src/host.rs` `//!`, `docs/p4-host-uart-findings.md` |
| `fan stopped` (survey flags status rows with `fan_duty` ≥ 20 and `fan_rpm` 0) | No tach pulse over a whole window at a duty that always turns the fan (`FanDuty::TURNS_FROM`): unplugged from the carrier's fan header, or stalled. `fan_rpm` NULL is the bridge's first second, or the window after a duty change, not a stop. Below 20 % the fan may stop by design (it needs 15 % to start from rest), so nothing is flagged. | `firmware/p4-bridge/src/fan.rs` `//!`, `crates/wartui-proto/src/fan.rs`, `docs/node-bus.md` § "On the carrier board" |
| `garbled` | COBS or link frames that did not decode, usually resynchronization after a truncated USB write. | `crates/wartui-proto/src/link/frame.rs` |
| `undecodable` | Records whose body would not parse for their type: always our bug. | `crates/wartui-core/src/engine/rx.rs`, `crates/wartui-proto/src/bus/` |
| `incompatible` | Records whose body is not their type's length, or sightings from a node whose heartbeat named another major or minor: a node on another build. The fix is to update the fleet together with `wartui ota`. | `crates/wartui-proto/src/bus/mod.rs` `BUS_VERSION`, `NODE_VERSION` |

## Sightings and decode

| Symptom | Usual meaning | Read |
|---|---|---|
| A channel outside the scan table (0, odd 5 GHz numbers) | The node takes the channel from the frame's DS Parameter Set or HT Operation element, and replaces any element channel that is not in the scan table, or that the radio could not have heard from the parked channel, with the parked channel. It keeps any 2.4 GHz table channel when parked on 2.4 GHz, and any table channel within 160 MHz on 5 GHz. An off-table channel in a capture points at a bug in `beacon.rs` channel parsing. | `crates/wartui-proto/src/beacon.rs` (channel parsing) |
| 5 GHz rows on a channel never assigned to the node | Usually an adjacent or wide-BSS beacon heard while parked nearby. A 5 GHz row more than 160 MHz from where the node was parked is a bug in `beacon.rs`. It is also a bug if a node repeatedly reports a whole band it was not given; check `park`. | `firmware/node/src/radio.rs` `park`, `crates/wartui-proto/src/plan/mod.rs` |
| No 5 GHz rows at all | Dual band failed to enable, which the node only logs as `could not enable dual band`, or the regulatory domain is wrong. | `firmware/node/src/main.rs` (`set_band_mode`); AGENTS.md (the `US` domain rule) |
| Zero BSSID, or the multicast bit set on a BSSID | Parsing took the wrong address field, or the frame was not a beacon or probe response. | `crates/wartui-proto/src/beacon.rs`, `firmware/node/src/sniff.rs` |
| Security `[UNDEFINED]` | No RSN or WPA element, and the privacy bit is ambiguous. Check which frame types produce it. | `crates/wartui-proto/src/beacon.rs` |
| Unknown `kind` | A kind this build does not know. The export skips it. | `crates/wartui-core/src/export.rs`, `crates/wartui-core/src/store.rs` (kind mapping) |
| A node heartbeats but stores no sighting for minutes | The node is idle with no assignment, or its channel change did not stick, or the area really was empty. Compare with other nodes in the same window. If they saw networks, it is the node. | `firmware/node/src/radio.rs` `park`, `crates/wartui-core/src/engine/node.rs` |

## Host, GPS and clock

| Symptom | Usual meaning | Read |
|---|---|---|
| Gaps over 15 s between `host_status` rows | The host process stalled, the machine slept, or power dipped. | `crates/wartui-core/src/runtime.rs` |
| Pi under-voltage or throttling | Supply sag. Expect the bridge or USB to misbehave in the same minutes. | `crates/wartui-core/src/health.rs` |
| `pos_source = none` stretches | GPS lost its fix, or the receiver was not found. Positions resolve per record through `PositionChain`. Never suggest `--lat/--lon` as a fallback. | `crates/wartui-core/src/position.rs`, `crates/wartui-core/src/gps.rs`, `crates/wartui-core/src/discover.rs` |
| Fix age above `gps_max_age_ms` | The chain handed out a stale fix, which would be a bug. | `crates/wartui-core/src/position.rs` |
| Position jumps above 250 km/h | NMEA glitches or a cold-start fix. The exported rows carry the bad position. | `crates/wartui-core/src/nmea.rs`, `crates/wartui-core/src/gps.rs` |
| `rx_at` steps backwards | The host wall clock was set mid-run by NTP or GPS time. Analysis walks rows by `id` for this reason, and the peak windows restart there. | `crates/wartui-core/src/analyze.rs` `//!` |
| Every node restarts within seconds while the bridge stays up | A shared supply to the nodes was cut, for example engine off or a socket. A panic on all boards at once is very unlikely. The heartbeat's `reset_cause` names each boot (the survey's `boots` line and `restart near` line): `power_on` confirms a cut supply, `brownout` a sagging one, and `software`, `watchdog` or `lockup` the firmware. | `crates/wartui-core/src/engine/rx.rs` (reboot detection), `crates/wartui-proto/src/reset.rs` `ResetCause`, `firmware/common/src/lib.rs` `reset_cause` |
| A node's free heap falls steadily across a boot (survey flags `free heap fell`; its `heap free` line prints first, min and last) | A leak in the node firmware. Only the radio's blobs allocate, so the figure should hold level once the node runs. A restart refills it, so compare within one boot. | `firmware/node/src/main.rs` `heartbeat`, `crates/wartui-proto/src/bus/heartbeat.rs` `Heartbeat::heap_free` |
| `battery_ma` negative throughout, voltage falling | The host ran on its own battery and was not charging. This affects range, not data. | `crates/wartui-core/src/health.rs` |
| `upload.result` is `unfollowed` | The host stopped waiting before the site said done or failed. Whether it imported is unknown; check the job on the site. Never re-upload without asking. | `crates/wartui/src/upload.rs` |
| An uploaded row count differs from what `wartui export` gives today | The build that uploaded exports differently from this one, for example one that keeps zero or group BSSIDs. Find the rows that differ before blaming the site. | `crates/wartui-core/src/export.rs` `//!` |
| `ended_at` NULL | The host died or was killed before shutdown. Check the log tail. | `crates/wartui-core/src/store.rs` |
