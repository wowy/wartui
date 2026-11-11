# C6 Bluetooth host link — a phone as the wartui host

Measured on 2026-11-10. The question was whether a phone can stand in for the PC as the wartui
host, over Bluetooth through the ESP32-P4-WIFI6's on-board C6, at the bar agreed on 2026-10-30:
at least 16 KB/s of application throughput, about 10× the 20-node average
([`superpowers/plans/2026-10-30-p4-uart-hci-spike.md`](superpowers/plans/2026-10-30-p4-uart-hci-spike.md)
has the bar and the load figures).

The code is on the branch `spike/c6-ble-host`, which is never merged: `9b23c96` to `83c1e25`.

## The split moved to the C6

The 2026-10-30 plan had the C6 run only as an HCI controller and the P4 run the Bluetooth host
stack. That puts raw H4 on UART1, and the radio link's core is frozen there
([`p4-uart-link-findings.md`](p4-uart-link-findings.md)): any P4 must identify and update any C6.
So the spike ran the host stack on the C6 instead:

- **The C6** runs TrouBLE and the GATT service on esp-radio's own controller.
- **The P4** sends it the COBS-framed `BridgeToHost` stream the CH343 carries, as new `radio_link`
  messages inside `Kind::Messages`, and takes back the `HostToBridge` stream a central writes. The
  core did not change.
- **Pacing:** a credit window of 16 KB: the P4 sends only what the C6 has room to notify, so a
  slow central backs data up in the P4's host queue and never holds the link behind RTS/CTS.
- **One owner for the host queue:** a subscribed central takes it, and any UART0 frame takes it
  back, so `flash-bridge` always gets in and out.

`flash-bridge` carried each spike build in and back out. On 96:C4, `main` → spike → `main` →
spike crossed with a confirmed trial each time, and no image advertised on `main`.

| | |
| --- | --- |
| `trouble-host` | 0.8.0, the newest release. Its `bt-hci` 0.10 `Transport` is a re-export from `bt-hci-transport` 0.1, which esp-radio 1.0.0-beta.1 already implements for `BleConnector`, so it needs no patch |
| C6 static SRAM | 134,796 B, plus a 96 KB heap whose high-water mark reached 37,976 B |
| C6 image | 462,992 B of a 1,966,080 B slot |
| GATT | one service, Up (notify) and Down (write without response); UUIDs in `wartui_proto::spike` |
| Advertised name | `wartui XX:YY` from the station MAC; the advertising address was the station MAC with the locally administered bit set and the last octet plus 1 (`98:A3:16:81:96:C4` → `9A:A3:16:81:96:C5`) |

## The setup

- **Boards:** 96:C4, a bare ESP32-P4-WIFI6 with no carrier, for runs 1 to 9. D0:1C, the
  carrier-10 P4 with ten C5 nodes on firmware 0.4.0, for run 10.
- **The Linux stand-in:** `tools/ble-standin` on the bench PC, through BlueZ on a USB adapter
  (`0489:e10d`). It reassembles and decodes with `wartui-proto`'s own code and checks every
  generated record's sequence and CRC.
- **Phones:** nRF Connect on each, which cannot check records, so their rates are the C6's
  counters over the time a device held the stream.
- **Load:** a generator on the P4 (`drive`, `fleet20`, `saturate`, `burst`), since 96:C4 has no
  nodes. `saturate` keeps the bulk ring within two frames of full.

## Two upstream bugs, both worked around without a patch

**esp-radio 1.0.0-beta.1 breaks every fragmented ACL packet.** `send_packet` in `ble/npl.rs`
zeroes the high byte of each outgoing ACL header, which forces every fragment's packet-boundary
flag to "start". A notification larger than one ACL packet reaches the central as unrelated
starts. Linux logged `Bluetooth: Unexpected start frame` for each one, and the stream stopped
within 2 s at ATT MTU 517. Light load worked only because nothing fragmented. The spike caps
TrouBLE's pool at 251 bytes, ATT MTU 247, so no notification fragments: 244 bytes of stream per
notification instead of 514.

**bt-hci 0.10.1 panics on a central's parameter request.** It declares LE Remote Connection
Parameter Request Reply (and its negative reply) with no return parameters, and the controller
returns the connection handle. A Pixel 8 sends LL_CONNECTION_PARAM_REQ right after connecting, so
every Pixel connection rebooted the C6. The captured panic was
`bt-hci-0.10.1/src/controller.rs:359:50: range end index 2 out of range for slice of length 0`.
iOS and BlueZ never send the request. Without `trouble-host`'s `connection-params-update` feature
the event stays masked and the controller answers the request itself; the Pixel then stayed
connected and set its own parameters. The C6's own interval request still goes out over L2CAP.

## Run 1 — the P4 feeds the C6 at 379 KB/s

A C6 build that credits the stream at once and runs no Bluetooth: 55.0 MB in 145 s, 379 KB/s, with
zero CRC, magic and UART errors at both ends. 4 Mbaud 8N1 carries at most 400 KB/s, so the FIFO
path runs near the line. The link to the C6 is not what limits Bluetooth.

## Linux throughput depends on the interval and the PHY

`saturate` for 120 s to the stand-in, every record checked:

| PHY | Interval | Throughput | Records, missing |
| --- | --- | --- | --- |
| 2M | 30 ms (BlueZ's choice) | 24.4 KB/s | 2,815, 0 |
| 2M | 15 ms (C6's request) | 32.5 KB/s | 3,756, 0 |
| 1M | 30 ms | **8.1 KB/s** | 932, 0 |
| 1M | 15 ms | 16.3 KB/s | 1,876, 0 |
| LE Coded (S8 requested) | 15 ms | 1.8 KB/s | 203, 0 |

At 1M this adapter took one notification per connection event, so throughput followed the event
rate. At 2M it took about three. Runs 7 to 9 also negotiated 15 ms on the default image, which
asks for no interval: inferred to be BlueZ reusing the 15 ms the C6 requested in the run before,
not confirmed.

## Run 4 — every device cleared the bar at MTU 247

`saturate`, 2 minutes or more each, default image, each device on its own choice of interval:

| Device | OS | PHY | Interval | MTU | Throughput |
| --- | --- | --- | --- | --- | --- |
| iPhone 17 | iOS 27.2.1 | 2M | 30 ms | 247 | ~48 KB/s |
| iPad mini (A17 Pro) | iPadOS 27.2 | 2M | 30 ms | 247 | 38.3 KB/s |
| Pixel 8 | Android 17 | 2M | 48.75 ms | 247 | ~57 KB/s |
| Pixel 8, no MTU request | Android 17 | 2M | 48.75 ms | 23 | ~6.8 KB/s |
| Galaxy Z Flip 8 | not recorded | 2M | 30 ms | 247 | ~85–90 KB/s |

- **How the figures were reached:** `~` figures are inferred from the C6's notified-byte counter
  over the time the user reported. The Flip's is an upper bound, since it includes a short first
  connection.
- **Android needs the app to ask for an MTU.** nRF Connect on Android kept MTU 23 until asked, so
  each notification carried 20 bytes. The Pixel still packed about 20 of them into each event.
- **Writes:** every device's Down writes reached the P4. nRF Connect on iOS once sent a frame
  without its trailing `00`, which the P4 merged with the next and dropped. A leading `00` on
  each hand-typed frame avoided it.
- **Two Pixel drops after an MTU request** were not reproduced: a later 6.5-minute run at MTU 247
  answered all 15 GATT requests and never dropped. The screen had gone to sleep before the first.

## Runs 5 and 6 — attaching and detaching

- **Clean detaches (run 5):** `fleet20` for 10 minutes to the stand-in with three reconnects. Every
  attachment began with `Ready` and a generator sequence from 0. 14,996 records arrived with no
  gap and no backlog.
- **Supervision timeout (run 6):** the Pixel walked out of range under `fleet20`.
  - The C6 recorded reason `0x08`, did not restart, and advertised again.
  - After the reconnect the generator's sequence started from 0, so nothing queued for the absent
    phone arrived (inferred from the sequence count).
  - 60 records were evicted, all counted. That is inferred to be the near-full ring left by the
    preceding `saturate`, filling as the link faded and before the timeout fired.
  - nRF Connect cannot show whether `Ready` came first; run 5 checked that path on the stand-in.

## Runs 7 to 9 — soak, bursts and command latency (96:C4, 2M, 15 ms)

- **Soak (run 7):** `saturate` for 10 minutes, 32.5 KB/s. 18,770 records arrived with no gap, and
  the link had zero errors at both ends. 96:C4 has no node bus, so this is the link alone.
- **Command latency (run 9):** 1,000 `GetStatus` round trips under `fleet20`. All 1,000 were
  answered, with p50 23.8 ms, p99 29.7 ms and max 34.2 ms. At 2M and 30 ms, 20 round trips had
  p50 38.8 ms and p99 58.9 ms.
- **Bursts (run 8):** the ring drained at about 30 KB/s. Stats arrive every 2 s, so peaks and
  times are to the nearest sample.

| Ring | Burst | Peak backlog | Evicted | Back to steady |
| --- | --- | --- | --- | --- |
| Internal, 240 KB | 132 | ~92 KB | 0 | ≤4 s |
| Internal, 240 KB | 264 | ~197 KB | 0 | ≤8 s |
| Internal, 240 KB | 500 | full | 220 records, all counted | ≤8 s |
| PSRAM, 4 MB | 132 | ~51 KB | 0 | ≤4 s |
| PSRAM, 4 MB | 264 | ~212 KB | 0 | ≤8 s |
| PSRAM, 4 MB | 500 | ~416 KB | 0 | ~16 s |

The PSRAM ring is 4 MB of the PSRAM the node image cache leaves, and freed 240 KB of internal
RAM. The internal-ring 500 burst's 220 missing records equal the P4's eviction counter. The
stand-in's own reconcile read 114 of them as unexplained, because its first stats record arrived
mid-burst.

## Run 10 — the real node bus (D0:1C, ten nodes, 2M, 30 ms)

10-minute arms of a real scan, with nodes on firmware 0.4.0:

| Arm | Throughput | Batches, lost | Heartbeats missed per node | C6-side CRC failures |
| --- | --- | --- | --- | --- |
| no load (first after flashing) | 1.1 KB/s | 176, 16 | ~6–7, about 20 s gaps on all ten (inferred from timing) | 1 |
| `saturate` | 24.3 KB/s | 205, 0 | ~0 | 0 |
| `fleet20` | 2.6 KB/s | 218, 0 | ~0–2 | 5 |
| no load again | 1.2 KB/s | 210, 0 | ~0 | 0 |
| `saturate`, no scan | 24.4 KB/s | idle nodes | 588–590 of ~600 | 0 |

- **Bluetooth load did not starve the bus.** No loaded arm lost a batch.
- **The first arm's 20 s gaps did not recur.** They hit every node at once and came with all 16
  lost batches, but the repeat arm, with timestamped events, had neither. The cause was not found.
- **The P4 → C6 direction lost 6 packets in the first 30 minutes and none in the 20 after.** Each
  CRC failure at the C6 matched a sequence gap. 96:C4 had none over hours of `saturate`. Running a scan or not did not decide it, and the cause
  was not found.
  - A lost packet drops stream bytes. At `fleet20` rates a packet usually holds one whole frame,
    so it drops cleanly (inferred). The one record missing with no eviction in the `fleet20` arm
    fits that.
  - Bytes lost this way are never credited, so the window shrinks by their size.
- **`saturate`'s evictions** (7 with a scan, 68 without) come from the generator keeping the ring
  within two frames of full: real frames arriving on top evict. No load record was lost to them
  in the no-scan arm.

## Against the pass criteria

| Criterion | Result |
| --- | --- |
| 1. Link clean for 10 minutes at full load with the buses running | **Not met.** 6 P4 → C6 CRC failures with D0:1C's bus running; 0 on 96:C4, which has no bus |
| 2. At least 16 KB/s | Met at 2M on every device and the stand-in, at MTU 247. Not met at 1M with a 30 ms interval, or at MTU 23 |
| 3. Same data, in order, no gaps | Met on 96:C4. On D0:1C one record was lost to a P4 → C6 packet loss (inferred) |
| 4. Commands work | Every device's writes reached the P4; p50 23.8 ms, p99 29.7 ms on the stand-in |
| 5. Attach and detach | Met for clean detaches and a supervision timeout |
| 6. No starvation | Met: Bluetooth load cost the node bus nothing |

## Workarounds a production build needs

Each one cost a hardware run to find. The spike commit that applies it is named, so its diff is
the recipe.

| Workaround | Why | Where on `spike/c6-ble-host` | Drop it when |
| --- | --- | --- | --- |
| Cap TrouBLE's packet pool at 251 bytes (`TROUBLE_HOST_DEFAULT_PACKET_POOL_MTU = "251"`, ATT MTU 247), and ask for data length 251 | esp-radio 1.0.0-beta.1 marks every ACL fragment as a start; any notification longer than one ACL packet is dropped by the central and the stream stalls | `8b40dd5`, `firmware/p4-radio/.cargo/config.toml` | esp-radio's `send_packet` (`ble/npl.rs`) stops zeroing the ACL header's high byte |
| Leave `trouble-host`'s `connection-params-update` feature off | bt-hci 0.10.1 declares no return for LE Remote Connection Parameter Request Reply and Negative Reply; a central's LL_CONNECTION_PARAM_REQ (every Android phone tested) panics the C6 | `02b12ca` | bt-hci declares `Return = ConnHandle` for opcodes 0x2020 and 0x2021, in a release `trouble-host` accepts |
| Ask for a connection interval through `update_connection_params`, which goes out as an L2CAP request without that feature | the feature above is what TrouBLE would use for the link-layer procedure | `0473202` | as above |
| A phone app requests an MTU after connecting | Android stays at MTU 23 until the app asks: 20 bytes a notification, ~6.8 KB/s on the Pixel 8 | — | never: it is how Android works |
| A phone app starts each write to Down with a `0x00` | a frame that arrives without its terminator merges with the next, and both are lost; a leading `0x00` ends any partial frame first | — | never: it costs one byte |
| Keep the C6's last panic text in persistent RTC memory and report it at the next boot | the panic handler only reboots, and nothing else on the board records why; without it the bt-hci panic needed a serial adapter on header H4 | `889585c`, `firmware/p4-radio/src/last_panic.rs` | never: it is cheap and the only record |
| Run the C6 on `trouble-host` 0.8.0 directly over `BleConnector` | esp-radio already implements `bt-hci-transport` 0.1's `Transport`, which `bt-hci` 0.10 re-exports; no byte adapter, no patch | `f8994d9` | — |

Two design points the spike leaves for production:

- **A lost P4 → C6 packet is not recovered.** `HostQueue::fill` splits frames across packets, so a
  lost `Down::HostBytes` garbles the frame it cuts, and its bytes are never credited, so the credit
  window shrinks by their size for the rest of the connection. Run 10 lost 6 such packets in 50
  minutes. A production link needs either a resend or a credit that resynchronizes.
- **Benchmarks against BlueZ need its cache cleared.** After the C6 requested 15 ms once, later
  connections to the same address came up at 15 ms on an image that requests nothing (inferred,
  not confirmed). Remove the device in `bluetoothctl` before comparing intervals.

## What this settles

- A phone can be the wartui host through the C6 without touching the radio link's frozen core,
  and `flash-bridge` moves a board on and off such a build.
- Throughput at 2M and MTU 247 is 38–90 KB/s on the four test devices: 2.4–5.6× the bar.
- The P4's 240 KB ring evicts on a 500-batch burst at Bluetooth rates. A 4 MB PSRAM ring does not.
- Bluetooth load does not starve the node bus.

It does not settle:

- **Why P4 → C6 packets fail on D0:1C.** Intermittent, and not tied to scanning in this data.
- **Why the first D0:1C arm lost 16 batches** to 20 s pauses on every node at once.
- **The stall after a broken session.** Before the MTU cap, the C6 stopped advertising after the
  fragmented session ended, while a clean session's end always re-advertised. It was not seen
  again once nothing fragmented.
- **Down-direction throughput,** which matters for updates from a phone. Only single frames were
  written. A P4 image of about 431 KB would take about 9–18 s at the up rates measured here
  (inferred, not measured).
- **Pairing, bonding and encryption,** and a phone app of our own, which must ask for an MTU on
  Android.
