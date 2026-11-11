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

`flash-bridge` carried each spike build in and back out.

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

## Run 2 — Bluetooth up, and the way back

- **Up:** the stand-in connected at 2M, 30 ms and MTU 247. `Ready` came first, and 20 `GetStatus`
  round trips were all answered, with p50 38.8 ms and p99 58.9 ms.
- **The way back:** on 96:C4, `main` → spike → `main` → spike crossed with a confirmed trial each
  time, and no image advertised on `main`.

## Run 3 — Linux throughput depends on the interval and the PHY

`saturate` for 120 s to the stand-in, every record checked:

| PHY | Interval | Throughput | Records, missing |
| --- | --- | --- | --- |
| 2M | 30 ms (BlueZ's choice) | 24.4 KB/s | 2,815, 0 |
| 2M | 15 ms (C6's request) | 32.5 KB/s | 3,756, 0 |
| 1M | 30 ms | **8.1 KB/s** | 932, 0 |
| 1M | 15 ms | 16.3 KB/s | 1,876, 0 |
| LE Coded (S8 requested) | 15 ms | 1.8 KB/s | 203, 0 |

At 1M this adapter took one notification per connection event, so throughput followed the event
rate. At 2M it took about three at 30 ms and about two at 15 ms. Runs 7 to 9 also negotiated
15 ms on the default image, which asks for no interval: inferred to be BlueZ reusing the 15 ms
the C6 requested in run 3's 2M arm just before, not confirmed.

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
  each notification carried at most 20 bytes. The Pixel still packed ~20 of them into each event,
  ~17 B each on average (inferred from the notification count over the run).
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
  - 60 records were evicted, all counted. The cause was not found. `saturate` had left the ring
    nearly full when `fleet20` started, but at the Pixel's ~57 KB/s that drains within seconds, so
    it explains them only if the link was already fading at the switch.
  - nRF Connect cannot show whether `Ready` came first; run 5 checked that path on the stand-in.

## Runs 7 to 9 — soak, bursts and command latency (96:C4, 2M, 15 ms)

- **Soak (run 7):** `saturate` for 10 minutes, 32.5 KB/s. 18,770 records arrived with no gap, and
  the link had zero errors at both ends. 96:C4 has no node bus, so this is the link alone.
- **Command latency (run 9):** 1,000 `GetStatus` round trips under `fleet20`. All 1,000 were
  answered, with p50 23.8 ms, p99 29.7 ms and max 34.2 ms. Run 2 gives the 30 ms figures.
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
| no load, disturbed (see below) | 1.1 KB/s | 176, 16 | ~6–7, in two gaps on all ten (inferred from timing) | 1 |
| `saturate` | 24.3 KB/s | 205, 0 | ~0 | 0 |
| `fleet20` | 2.6 KB/s | 218, 0 | ~0–2 | 5 |
| no load again | 1.2 KB/s | 210, 0 | ~0 | 0 |
| `saturate`, no scan | 24.4 KB/s | idle nodes | 588–590 of ~600 | 0 |

- **Bluetooth load did not starve the bus.** No loaded arm lost a batch.
- **The first arm's gaps were the test's own doing.** Two commands aimed at 96:C4 ran during it:
  `flash-bridge --bridge 96:C4`, 8 s in, and `status --bridge 96:C4`, 52 s in. `--bridge` finds a
  board by sending `Identify` to every CH343 on the PC (`named_board`), so D0:1C's UART0 got one
  each time.
  - A UART0 frame takes the queue from Bluetooth and clears it. Nothing read UART0, so its gate
    closed after `HOST_SILENCE` (15 s), and Bluetooth took the queue back, clearing it again.
  - The arm's record shows both: three `Ready`s instead of one, and the P4's count of time with
    Bluetooth owning the queue stopped for about 15 s at each of those two moments, while the
    stand-in received nothing.
  - What was queued in those windows was lost from every node at once: the 16 batches, and at one
    beat per 5 s, the 6–7 heartbeats per node. The bus lost nothing, and the repeat arm, with no
    other command running, lost no batch.
- **The P4 → C6 direction lost 6 packets in 50 minutes.** Each CRC failure at the C6 matched a
  sequence gap, with no UART error. 96:C4 had none over hours. Run 11 found the cause.
  - A lost packet drops stream bytes, and bytes lost this way are never credited, so the window
    shrinks by their size. The one record missing with no eviction in the `fleet20` arm fits a
    lost packet (inferred).
- **`saturate`'s evictions** (7 with a scan, 68 without) come from the generator keeping the ring
  within two frames of full: real frames arriving on top evict. No load record was lost to them
  in the no-scan arm.

## Run 11 — what the lost P4 → C6 packets were (D0:1C)

The C6 was built to keep each failed packet's bytes as they arrived, work out which single-byte
change would make it verify, and record where its DMA reads split it (`193bff4`, `6f4ce49`).
Five 20-minute arms ran with nothing else touching the PC's serial ports:

| Arm | Failed P4 → C6 packets |
| --- | --- |
| no load, nodes scanning | 4 |
| no load, nodes idle | 6 |
| `fleet20`, nodes scanning | 1 |
| `fleet20`, nodes idle | 24, then 9 in a repeat |

- **Every failure whose report arrived, 32 of 44, is two packets merged by a lost terminator.**
  The reports travel back through the same link, so some were lost with it (inferred). A 120-byte
  frame is exactly two 60-byte packets with nothing between them: the `0x00` that ends the first
  is gone, not changed into another byte, and the C6 next sees the packet after the second. No
  stray byte, no flipped bit, no UART error, so it is not line noise.
- **The byte goes missing at the end of a C6 read.** In all six reports from the repeat arm, one
  DMA read ended on the previous packet's `0x00`. The next read held exactly the failed
  packet minus its terminator, and it ended on an idle line, not at its size limit. The read after
  that started with the next packet.
  - So the P4 paused one byte short of the packet's end, long enough for the C6 to end the read.
    The lone `0x00` it then sent never reached the C6's memory.
  - 21 reads ended mid-packet in that arm and 9 packets failed, so at least 12 mid-packet ends
    lost nothing. Every loss seen was a single trailing byte.
- **Where each half comes from:**
  - The P4 writes a packet in pieces as FIFO space frees up (`write_async`). A pause before the
    last piece needs the P4 to be late refilling the FIFO, and only D0:1C's P4 is also serving a
    node bus's interrupts (inferred). More small packets, as idle nodes beating every second
    send, give it more chances.
  - The C6 ends each read on an idle line and arms the next. Arming resets the receive DMA channel
    (esp-hal 1.2.2 `ChannelRx::do_prepare`). Run 12 shows that dropping the re-arm stops the loss.

## Run 12 — one receive transfer that never stops (D0:1C)

The C6 was changed to receive through a `DmaRxStreamBuf` (`cd7d94d`): one UHCI transfer armed at
boot and never stopped, 32 descriptors of 256 bytes, each closed by an idle EOF, polled every
250 µs and handed back to the DMA as it is read. It would restart only if the DMA ran out of
descriptors, and counts each restart. It costs 428 bytes of the C6's RAM. The change is inside
the C6's own receive, so the link's frozen wire stays as it is.

| Arm | Build | Failed P4 → C6 packets | Load records lost | Throughput |
| --- | --- | --- | --- | --- |
| `fleet20`, nodes idle, 20 min | stream | 0 | 0 of 30,000 | 2.9 KB/s |
| `saturate`, nodes scanning, 10 min | stream | 0 | 0 of 65,059 | 113.7 KB/s |
| `saturate`, nodes scanning, 10 min | previous (`6f4ce49`) | 6 reported | 21 of 21,175 | 36.4 KB/s |

- **The loss is gone.** The same `fleet20` arm lost 24 and then 9 packets on the previous build.
  The stream build never restarted its transfer, and saw no UART error.
- **The previous build lost packets under `saturate` too,** this time as large merged frames: half
  its reads, 10,627 of 21,002, ended mid-packet. Its C6 counters read 0 at the end of the arm
  while it reported 6 failures; why was not found.
- **The stream build carried 3.1× the previous build's throughput,** on the same PC, stand-in,
  MTU 247, 2M and 30 ms interval, run back to back: about 14 notifications per connection event
  against about 4.5. Why the old receive held Bluetooth back was not found. The stand-in figures in
  runs 3 to 10 were all taken on the old receive, so they understate what the C6 can carry.
- **`main`'s C6 receives the same way the previous build did,** re-arming after every read. Its
  P4 → C6 traffic is small, apart from updates.

## Run 13 — the P4's host receive does not lose the byte (D0:1C)

The P4 reads the host's UART0 the way the C6 used to read its link: two DMA buffers, the next read
armed as each one ends (`firmware/p4-bridge/src/host.rs`). D0:1C ran `main` (`56af2ae`), with no
firmware change. A program on the PC sent `GetStatus` one frame at a time through the CH343 and
waited for each `Status`, so a frame that lost a byte would go unanswered. Most frames were written
in two parts:

- the last byte alone, 2 ms or 20 ms after the rest had drained, far past the idle that ends a
  read, so the terminator had a read of its own (inferred: the PC cannot see the P4's reads)
- the last byte after a gap of 0 to 2 ms, which USB timing scatters around the moment the P4 arms
  its next read
- the last 2 or 3 bytes after 2 ms, and whole frames, for comparison

| Arm | Frames | Answered | Split before the terminator |
| --- | --- | --- | --- |
| nodes scanning, 10 min | 63,691 | 63,691 | 49,539 |
| nodes idle, 10 min | 65,417 | 65,417 | 50,881 |

- **No frame lost a byte,** in any split. `host_uart_errors` stayed 0, nothing came back garbled,
  and the slowest answer took 3 ms.
- **So the loss is the C6's, not the pattern's.** The same re-arm on the P4's own UHCI and DMA
  keeps a lone terminator. Why the two chips differ was not found. This agrees with the 2026-11-05
  host link bench, whose 17,164 reads at 4 Mbaud lost no frame (`docs/p4-host-uart-findings.md`).
- The P4 still loses host bytes while it erases or writes its own flash, as that bench found. That
  is interrupts held off, not the re-arm.

## Against the pass criteria

| Criterion | Result |
| --- | --- |
| 1. Link clean for 10 minutes at full load with the buses running | **Not met.** 6 P4 → C6 CRC failures on D0:1C, in the no-load (1) and `fleet20` (5) arms; none in the 10-minute `saturate` arm. 0 on 96:C4, which has no bus. Each is a packet terminator the C6 loses when the P4 pauses just before it (run 11). The stream receive lost none (run 12) |
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
| Leave `trouble-host`'s `connection-params-update` feature off | bt-hci 0.10.1 declares no return for LE Remote Connection Parameter Request Reply and Negative Reply; a central's LL_CONNECTION_PARAM_REQ panics the C6 (the Pixel 8 sent one on every connection; the Flip 8 ran only after the fix) | `02b12ca` | bt-hci declares `Return = ConnHandle` for opcodes 0x2020 and 0x2021, in a release `trouble-host` accepts |
| Ask for a connection interval through `update_connection_params`, which goes out as an L2CAP request without that feature | the feature above is what TrouBLE would use for the link-layer procedure | `0473202` | as above |
| A phone app requests an MTU after connecting | Android stays at MTU 23 until the app asks: 20 bytes a notification, ~6.8 KB/s on the Pixel 8 | — | never: it is how Android works |
| A phone app starts each write to Down with a `0x00` | a frame that arrives without its terminator merges with the next, and both are lost; a leading `0x00` ends any partial frame first | — | never: it costs one byte |
| Keep the C6's last panic text in persistent RTC memory and report it at the next boot | the panic handler only reboots, and nothing else on the board records why; without it the bt-hci panic needed a serial adapter on header H4 | `889585c`, `firmware/p4-radio/src/last_panic.rs` | never: it is cheap and the only record |
| Run the C6 on `trouble-host` 0.8.0 directly over `BleConnector` | esp-radio already implements `bt-hci-transport` 0.1's `Transport`, which `bt-hci` 0.10 re-exports; no byte adapter, no patch | `f8994d9` | — |

Four design points the spike leaves for production:

- **The C6 receives on one transfer that never stops.** `cd7d94d` is the recipe. The same
  receive belongs on `main`'s C6, whose re-armed reads lose a terminator whenever the P4 pauses one
  byte short of a packet's end.
- **The P4's second core could serve the C6 link** (an option raised during the spike, not tried).
  The P4 pauses mid-packet when it is late refilling the UART FIFO, as it is while also serving a
  node bus. With the C6 fixed, pauses cost nothing but time, so this is about throughput under
  load, not loss. The host queue and link state are shared through `RefCell` today, so the move
  needs a critical-section mutex and an executor on core 1, and every P4 flash write parks
  core 1, which would stall the link for that write.
- **A lost P4 → C6 packet is not recovered.** `HostQueue::fill` splits frames across packets, so a
  lost `Down::HostBytes` garbles the frame it cuts, and its bytes are never credited, so the credit
  window shrinks by their size for the rest of the connection. Run 10 lost 6 such packets in 50
  minutes. A production link needs either a resend or a credit that resynchronizes.
- **A `wartui` command for another bridge takes the queue from a Bluetooth host.** Any `--bridge`
  lookup on a PC that a Bluetooth-hosted bridge is plugged into sends it `Identify`, which takes
  the queue and drops what was in it, then hands it back after 15 s and drops it again. A
  production build should answer `Identify` without taking the queue. Until then, nothing else
  that probes ports runs during a Bluetooth measurement.
- **Benchmarks against BlueZ need its cache cleared.** After the C6 requested 15 ms once, later
  connections to the same address came up at 15 ms on an image that requests nothing (inferred,
  not confirmed). Remove the device in `bluetoothctl` before comparing intervals.

## What this settles

- A phone can be the wartui host through the C6 without touching the radio link's frozen core,
  and `flash-bridge` moves a board on and off such a build.
- Throughput at 2M and MTU 247 cleared the bar on all four test devices: 38.3 KB/s measured on
  the iPad, and ~48 to ~90 KB/s on the others, inferred from counters (the Flip's is an upper
  bound). That is 2.4× the bar or more.
- The P4's 240 KB ring evicts on a 500-batch burst at Bluetooth rates. A 4 MB PSRAM ring does not.
- Bluetooth load does not starve the node bus.
- The first D0:1C arm's lost batches came from a port probe aimed at the other bridge, not the bus.
- Each P4 → C6 packet lost on D0:1C is a terminator the C6 drops when the P4 pauses one byte short
  of a packet's end, and a C6 receive that never re-arms loses none.
- The P4's host receive, re-armed the same way, keeps a terminator that arrives alone, so it
  needs no such change.

It does not settle:

- **Which step of the re-arm drops the byte,** the channel reset or the one-byte read that should
  follow, and why the P4 keeps it (run 13). The stream receive avoids both, so it does not matter
  for the fix.
- **Why the old receive held Bluetooth throughput at a third.**
- **Phone throughput on the stream receive.** Runs 3 to 10 were all on the old one.
- **The stall after a broken session.** Before the MTU cap, the C6 stopped advertising after the
  fragmented session ended, while a clean session's end always re-advertised. It was not seen
  again once nothing fragmented.
- **Down-direction throughput,** which matters for updates from a phone. Only single frames were
  written. A P4 image of about 431 KB would take about 5–18 s at the up rates seen here, from
  24 KB/s on the stand-in to ~90 KB/s on the Flip (inferred, not measured).
- **Pairing, bonding and encryption,** and a phone app of our own, which must ask for an MTU on
  Android.
