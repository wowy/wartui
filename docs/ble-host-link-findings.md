# The Bluetooth host link — bench acceptance on D0:1C

Measured on 2026-11-13 on D0:1C, the carrier-10 P4 with its ten C5 nodes, from the bench PC:
Fedora 44, kernel 7.3.3, BlueZ 5.88 on the USB adapter (`0489:e10d`). The PC's adapter held no
other connection during any run. This is the acceptance bench of the production link, which
`wartui --link bluetooth` uses. The spikes behind it are
[`c6-ble-host-findings.md`](c6-ble-host-findings.md) and
[`c6-ble-pairing-findings.md`](c6-ble-pairing-findings.md).

The functional checks ran on the production firmware at `adfe3aa` and the host at `43caeb3`. The
load checks ran on the branch `bench/ble-load` (`8652313`), which is never merged. It adds a load
generator to the P4, a `wartui ble-bench` command that reads the load through the production
transport, a command that resets the C6 alone, and a C6 build setting that discards every Nth
`Down::HostBytes`.

No phone ran that day. The Pixel and the iPad ran on 2026-11-15: see [The phones](#the-phones).
A drive from a HackberryPi followed the same day: see [A first drive](#a-first-drive). Updates
over Bluetooth ran the same day too: see [Updates over Bluetooth](#updates-over-bluetooth). A Mac
ran the bench on 2026-11-20: see [macOS](#macos).

## Results

| Check | Result |
| --- | --- |
| `wartui bluetooth pair` from scratch, then a run | Paired and bonded on the first try, in 17 s with no prompt. 15 s of that is the wait for USB to go quiet |
| 10-minute scan over Bluetooth, ten nodes | 0 batches lost, 0 frames lost on the link, 118 heartbeats from every node, no disconnection |
| `saturate`, 60 s | 127.6 and 127.3 KB/s read by the host. No loss on the link |
| 500-record burst | Drained in 3.6 s. 0 evicted, 0 lost |
| Credit resync, one P4→C6 packet in 50 discarded | The rate held at 127.4 KB/s for 30 s. Only the records each loss cut were lost |
| `wartui ports` during a Bluetooth run | The run lost nothing and logged nothing |
| `wartui status` over USB during a Bluetooth run | USB took the bridge. The run got the reason, and the bridge back 15.06 s later |
| Resume: disconnect, bridge reset, C6 reset, adapter off and on | Each resumed with no pairing prompt |
| A PC with no bond, window closed | Dropped after 10 s. It never took the bridge |

## The scan

A `wartui run --link bluetooth` of 10 minutes, the nodes scanning Wi-Fi on a quiet desk.

- **Nothing was lost.** The bridge received 1,387 node records and the host read 1,387. No batch
  gap, no duplicate, no garbled or undecodable frame.
- **Heartbeats were on time.** Every node sent 118, 4.89 to 5.13 s apart on a 5 s beat.
- **The link stayed up.** 118 status replies, about 5.2 s apart. No radio link error, no
  resynced byte, no dropped write.
- **The host ran up to 79 ms behind the bridge,** 23 ms at the median. A 30 ms connection
  interval puts a floor under it.
- The capture-review survey raised nothing about the link. Its two warnings were nodes that heard
  no access point on their channels.

## Throughput

The generator's `saturate` profile keeps the P4's 1 MB host ring full of 1,024-byte records. The
host counts the records it decodes, and the P4 counts the stream bytes the C6 credits.

| Link | Interval | Read by the host | Credited by the C6 |
| --- | --- | --- | --- |
| Bluetooth | 30 ms, the PC's own | 127.6 KB/s | 129.6 KB/s |
| Bluetooth | 30 ms | 127.3 KB/s | 129.4 KB/s |
| Bluetooth | 15 ms, asked by the C6 | | 112.9 KB/s |
| Bluetooth | 15 ms | | 113.3 KB/s |
| Bluetooth | 30 ms | | 127.6 KB/s |
| USB | | 389.1 KB/s | |

- **The link carries 16% more than the spike's ~110 KB/s** (run 15), at the same 30 ms interval.
- **The rate moves between two levels within a run,** about 144 KB/s and about 112 KB/s, for 10 to
  30 s each. The cause is not known.
- **Asking for 15 ms costs 13% on this PC.** The runs are in the order of the table, so the 30 ms
  figure holds on both sides of the 15 ms pair. The C6 keeps leaving the interval to the central.
- **Every missing record was one the P4 evicted.** A saturated run lost 2, 10 and 2 records, and
  the P4's `host-bulk` queue counted the same evictions each time. The generator stops short of a
  full ring, and the ten nodes' own records then tip it over. No record was lost between the ring
  and the host, and none failed its CRC.

**The burst.** 500 full records at 700 a second, about 520 KB in 0.71 s, then a twenty-node fleet's
steady 25 records a second. The host had all 500 after 3.6 s, 138 records a second. The ring
evicted nothing, and the 733 steady records behind the burst all arrived.

## The credit resync

A C6 built with `WARTUI_BENCH_DROP_HOST_BYTES_EVERY=50` discards one `Down::HostBytes` in 50, as
if the P4's packet were lost. `saturate` ran over it for 30 s.

- **The window did not shrink.** The C6 credited 127.4 KB/s, the rate of an undamaged link. The
  C6 discarded 131,164 bytes, eight times the 16 KiB window, so a credit that did not resync would
  have closed the window within 4 s (inferred).
- **Only the cut records were lost.** 84 gaps took 197 of 4,646 records, 1 to 3 each: a packet
  carries up to 2 KB of the stream, so its loss cuts up to three 1 KB records. The host counted
  165 undecodable frames, about two partial frames at each loss.
- **The C6 counted every byte** in `resynced_bytes`.
- One command's reply was among the cut frames: the bench's last `GetStatus` went unanswered. A
  real loss costs a host whatever frame it lands in, a reply included.

## Resuming

A `wartui run --link bluetooth` stayed up through each of these, and no pairing prompt appeared.

| Event | Link down to served again |
| --- | --- |
| `bluetoothctl disconnect` | 2.1 s, 1 s of it the transport's wait before it retries |
| `wartui reset` over USB, which restarts both chips | Connected again in 2.1 s, served after 15.3 s: the reset came over USB, so USB held the bridge for its 15 s |
| Adapter off for 4 s, then on | Served 5 s after the adapter came back |
| `wartui status` over USB (a takeover) | 15.06 s. The run lost what the nodes sent meanwhile: 31 frames, one sighting batch among them |

The C6 alone was reset through its enable pin with the bench command, with no run attached. A
`wartui status --link bluetooth` started straight after connected and was answered in 1.2 s. The
C6 had the bonds back from the P4 by then.

**The first connection of a command takes 2 to 11 s.** Three runs took 2, 7 and 11 s to subscribe.
The 11 s one was a connection attempt that timed out at 10 s and a retry that took 1 s. The cause
is not known.

## A PC with no bond

The PC forgot the bridge (`bluetoothctl remove`) with the pairing window closed, then ran `wartui
status --link bluetooth`.

- **The C6 dropped it** after its 10 s deadline: `unbonded_kicks` rose by one each time, and the
  disconnect reason was `0x16`, local terminated. It never took the bridge.
- **BlueZ started no pairing.** `failed_pairings` stayed 0.
- **The command gave up at 15 s without naming the cause.** The C6's drop can come after that.
  `b5614db` makes the notice ask BlueZ, and say first that the PC is not paired.
- `wartui bluetooth pair` paired it again in 17 s.

## BlueZ and a first pairing

[`c6-ble-pairing-findings.md`](c6-ble-pairing-findings.md) says BlueZ never started a first
pairing on its own. On this PC it does when a desktop session runs: with the window open, a first
connection raised the desktop's Bluetooth prompt, and the pairing completed once it was accepted.
Why the spike saw none is not known. The likely difference is which agent was registered
(inferred, not tested). `wartui bluetooth pair` registers its own agent and needs
no prompt.

## After the review fixes

`867de99` changes the C6: it repeats its `BleState` every second, and a central's write leaves
1 KiB of the queue to the P4 free. Part of the bench ran again on it, the load checks from
`bench/ble-load` at `797882a`.

| Check | Result |
| --- | --- |
| A run through a forced disconnect and a USB takeover | Served again 2.1 s after the disconnect, and 15.06 s after the takeover. No batch lost |
| `saturate`, 30 s | 126.6 KB/s read by the host, 128.4 KB/s credited. Nothing evicted or lost |
| 500-record burst | Drained in 4.7 s, at the lower of the two rate levels. Nothing evicted or lost |

## After a third review's fixes

`38e896d` changes the P4's host bulk ring: it copies a frame in two pieces, where it divided once
for every byte. The bench image with the change (`bench/ble-load` at `e89e843`) and the one before
it (`797882a`) ran in turn on D0:1C, `saturate` for 60 s over Bluetooth and 20 s over USB each
time. Three Bluetooth runs on the image with the change came first, at 144.5, 144.2 and
143.7 KB/s.

| Image | Bluetooth, read by the host | USB, read by the host |
| --- | --- | --- |
| Before | 144.3 KB/s | 389.1 KB/s |
| After | 144.2 KB/s | 392.3 KB/s |
| Before | 144.3 KB/s | 389.1 KB/s |
| After | 144.3 KB/s | 392.2 KB/s |

- **The change costs nothing and gains 0.8% over USB.** Bluetooth is bound by the radio, not by
  the P4.
- **Every run here held the upper of the two rate levels,** on both images: 144 KB/s
  throughout, and no 2 s window under 131 KB/s in seven Bluetooth runs. The first bench's
  127 KB/s runs mixed both levels. So the level lasts across runs and does not follow the build.
- **Every missing record was again one the P4 evicted.** The image after the change evicted 1 to 8
  records in five of its seven runs, and the one before it none in four. The first bench's runs
  of the earlier image evicted 2, 10 and 2.
- A run through a forced disconnect and a USB takeover on the image after the change was served
  again 2.1 s after the disconnect and 15.06 s after the takeover. It lost the 33 frames the nodes
  sent during the takeover, one sighting batch among them (inferred from when its gap showed).

## After a fourth review's fixes

`b25d52d` changes the C6 three ways: it stops its advertisement to apply a bond change, it
disconnects a connected central whose bond the P4 dropped, and it repeats its credit every second.
The checks ran on `bench/ble-load` on top of it.

| Check | Result |
| --- | --- |
| `wartui bluetooth forget --link usb` while the PC is connected over Bluetooth | The C6 disconnected the PC as the P4 cleared the bonds, reason `0x16`, and counted one `unbonded_kicks` |
| `wartui bluetooth forget --link usb` with no central connected | The C6 stopped advertising, loaded the empty list, and advertised again |
| `wartui bluetooth pair` after each | Paired and bonded in 18 s both times. The new bond's echo from the P4 disconnected nothing |
| `wartui status --link bluetooth`, by the remembered address and by a scan for `D0:1C` | 3.0 s each |
| C6 reset alone, then `wartui status --link bluetooth` | Answered in 3.0 s, bonded |
| `saturate`, 30 s | 143.1 KB/s read by the host, 145.0 KB/s credited. The 4 missing records were 4 evictions |
| A run through a forced disconnect and a USB takeover | Served again 2.1 s after the disconnect and 15.05 s after the takeover |

The bond change that matters most did not run: a bond that reaches the C6 while it advertises, for
a central on a resolvable private address. This PC uses a fixed address, so the resolving list
plays no part in recognizing it. That case rests on reading TrouBLE 0.8.0, which loads its
resolving list only while not advertising.

## After a fifth review's fixes

Measured on 2026-11-15, after the phones. `a54025e` changes both chips. The C6 repeats a new bond
until the P4 echoes it, and disconnects a bonded central that stays unsubscribed for 30 s. The P4
repeats its bonds to the C6 every 5 s. The checks ran on the production firmware at `a54025e`, and
the load on `bench/ble-load` on top of it (`e59424a`).

| Check | Result |
| --- | --- |
| A bonded PC connects and never subscribes (`bluetoothctl connect`) | Dropped 30 s after it connected, reason `0x16`. Not counted in `unbonded_kicks` |
| A run subscribed and waiting 45 s while a USB host holds the bridge | Not dropped. Served again once USB let go |
| The P4's bonds repeat, 75 s with nothing changing | The C6 logged no restart of its advertisement |
| `wartui bluetooth forget --link usb` while the PC is served over Bluetooth | The C6 disconnected the PC, reason `0x16`, one `unbonded_kicks` |
| `wartui bluetooth forget --link usb` with no central connected | Bonds cleared, the window open until a host pairs |
| `wartui bluetooth pair` after each | Bonded in 17.4 and 16.9 s. Two `status` runs after each connected on the bond, 3.0 s each |
| C6 reset alone, then `wartui status --link bluetooth` | 3.5 s by the remembered address, 3.0 s by a scan |
| A run through a forced disconnect | Served again 2.1 s after the link dropped |

**The saturated load first ran on a shared adapter.** The PC's adapter held a second connection,
to a mouse, and the board sat behind two USB hubs at 12 Mbit/s. So the image with the change and
the one from before the third review's (`797882a`) ran in turn.

| Image | Bluetooth, read by the host | USB, read by the host |
| --- | --- | --- |
| After | 16.1 KB/s | 391 KB/s, 0 to 3 undecodable frames a run |
| Before | 15.1 KB/s | 389 KB/s, 1 to 3 undecodable frames a run |

- **Both differ from the earlier sessions on both images alike,** so the cause is the setup and
  not the change. The earlier image ran 144 KB/s over Bluetooth and lost nothing over USB two
  days before.
- **The Bluetooth rate is two notifications in each 30 ms connection event,** 16.3 KB/s
  (inferred). The adapter fits the bridge's events between the mouse's.
- **Over USB, each run but one lost 4 to 13 records that the ring did not evict,** a few KB at a
  time, on both images.
- The repeats ran, but no run lost a bond message, so no run shows one recovering a loss.

**With the mouse off, the image with the change holds the earlier rate.** Two 60 s runs over
Bluetooth, the adapter to itself:

| Run | Read by the host | Credited by the C6 | Missing |
| --- | --- | --- | --- |
| 1 | 142.9 KB/s | 144.9 KB/s | 1 record, 1 eviction |
| 2 | 141.5 KB/s | 143.6 KB/s | 1 record, 1 eviction |

## The phones

Measured on 2026-11-15 on D0:1C, on `bench/ble-load` at `368d1af`, which sits on `b25d52d`. A
Pixel 8 and an iPad mini each ran nRF Connect. Each forgot the bridge first, since its bond dated
from the spike and the bridge no longer held it. A reset over USB opened the pairing window. The
bridge held the PC's bond throughout.

| Check | Pixel 8 | iPad mini |
| --- | --- | --- |
| Pairing in the window | Bonded. The phone asked twice | Bonded on the first tap |
| Link | MTU 247, 2M PHY, 48.75 ms | MTU 247, 2M PHY, 30 ms |
| `saturate`, two 75 s intervals | 73.8 and 75.8 KB/s | 38.7 and 38.9 KB/s |
| The spike's rate, encrypted | 67.9 KB/s | 38.2 KB/s |
| Lost on the link | 0 resynced bytes, 0 dropped writes | 0 resynced bytes, 0 dropped writes |
| Evicted from the full ring | 2 records | none |
| No bond, window closed | | Dropped. It never held the bridge |

Each rate is the credit's advance between two `ble-bench stats` reads over USB in mid-run, over
the time the phone held the bridge. A read takes the bridge for 15 s, so each run passed through
several takeovers. The phone was served again after each one between the reads, with no prompt.

- **The Pixel drains 9 to 12% more than on the spike, and the iPad what it drained there.** Each
  phone chose the interval it chose on the spike.
- **The Pixel's two prompts were one pairing.** The bridge counted 1 pairing, none failed, no
  unbonded kick and no disconnect. [`c6-ble-pairing-findings.md`](c6-ble-pairing-findings.md) put
  its two taps down to the 10 s deadline. The deadline is 30 s here and nothing failed, so the
  second prompt is the phone's own.
- **The iPad with no bond and the window closed** raised its own pairing prompt, and the link
  dropped as the prompt was accepted. The bridge counted one `unbonded_kicks`, reason `0x16`, and
  no failed pairing. It stored no bond, and the iPad never held the bridge.
- **An idle phone receives 2.3 KB/s.** ~1.7 KB/s of it is the bridge's GPS receiver's bytes (from
  a 20 s sniff over USB), and the rest the ten nodes' heartbeats, their scan stopped. The stream
  starts at the subscription: the bridge queues nothing while no host holds it.
- **The iPad's first write of the load command started nothing,** and no counter on the bridge
  says why. The same bytes written again started the load. The likely cause is the write's type
  in nRF Connect, or a write ahead of the subscription (inferred, not tested).

## A first drive

Measured on 2026-11-15: 2 h 14 min in a car, from a HackberryPi to bridge E6:A0, a carrier-10 with
its ten nodes scanning Wi-Fi. The host is built from `3fcb1d0`. The positions come from the
bridge's own GPS receiver, so its sentences cross the link too.

| Figure | The drive |
| --- | --- |
| Link | MTU 247, 1M PHY, 48.75 ms, encrypted and bonded in every status reply |
| Connections | One. No disconnect, and no status reply over 5.4 s after the one before |
| Node records | 65,078 received by the bridge, 65,078 read by the host. No batch gap |
| Lost on the link | 0 resynced bytes, 0 dropped writes, no radio link error |
| The P4's host ring | 1,539 bytes at its fullest, of 1 MB |
| Load | 78,127 sightings, 116 in the busiest second (2.4 KB of node records) |
| Lag behind the bridge | 73 ms at the median 5 s peak, 203 ms at worst |
| Fix age at a sighting | 253 ms at the median, under 1.5 s after the fix's first minute |
| The C6's heap high-water | 32.1 KB, 34.8 KB after 70 min, then level |

- **Nothing was lost,** and the one connection held for the whole drive.
- **The Pi's controller has no 2M PHY.** `btmgmt phy` lists `LE1MTX LE1MRX` alone, so the C6's
  request for 2M changes nothing there. No run measured the saturated rate on 1M.
- **The lag is delivery, not a backlog.** A Bluetooth link delivers once per connection interval,
  so frames arrive in clumps while the ring behind them stays empty. The lag reached 100 ms, the
  line a USB host is behind at, in 320 of 1,613 samples, and in 36 of 83 on a run with the scan
  stopped. Over Bluetooth the host's line is 500 ms for that reason
  (`crates/wartui-core/src/engine/lag.rs`, `behind_us`). The bench PC at 30 ms peaked at 79 ms.
- **The bridge's GPS works over the link.** The first fix came 6 min 21 s into the run, the bridge
  powered on as it began, and went stale once, for 13 s, in its first minute: 20 sightings have
  no position.

## Updates over Bluetooth

Measured on 2026-11-15 on D0:1C with its ten nodes, from the bench PC: BlueZ 5.88, MTU 247, 2M
PHY, a 30 ms interval, encrypted and bonded, and no other LE device on the adapter. The updates'
host is built from `746c919`, and the floods' from `main` with the throwaway command below. The
firmware is `main` at `fc359ed`, with throwaway instrumentation on the C6
that is never committed:

- counters in a private copy of esp-radio's receive path: ACL packets received, packets that
  arrived as a chain of buffers, and controller reads that failed, with the failing packet;
- TrouBLE's log;
- a throwaway `wartui` command that floods the bridge with update ops.

On 2026-11-13 every update over Bluetooth died at 0 to 21% (issue #314). TrouBLE's host on the C6
stopped with `Controller(Unknown)`, and the update's sender overran the link. `fdde94e` paces the
sender to the link. `746c919` lets `flash-bridge` and `ota` take it, and `flash-bridge` reads its
outcome across the reconnects.

### The host stop was not reproduced

Each run sent three floods of 1.5 MB, in 244-byte writes, each frame ending with a write request.

| Run | Target | Fleet | Result | Rate |
| --- | --- | --- | --- | --- |
| 1 | The P4's node image cache (PSRAM) | Idle | 3 of 3 verified | 17.1 KB/s |
| 2 | The P4's node image cache | Scanning | 3 of 3 verified | 17.1 KB/s |
| 3 | The P4's update receiver (flash, never installed) | Scanning | 3 of 3 verified | 15.3 KB/s |
| 4 | The P4's update receiver, ops back to back with no wait for a sector's status | Scanning | 3 of 3 verified | 16.6 KB/s |

- **18 MB crossed the link with nothing lost.** No link drop and no failed controller read, and
  TrouBLE's `rx_errors` stayed 0. Every flood's digest verified with no rewind, so the C6 dropped
  no write.
- **The controller never chained a packet.** Every ACL packet arrived in one buffer, up to the full
  255 bytes: 93,498 packets in one C6 life. The issue's lead holds in the source: esp-radio
  1.0.0-beta.1's `ble_hs_rx_data` copies only the first buffer of a chain, so a chained packet
  would reach TrouBLE truncated. This bench never exercised it.
- **What stopped the host on 2026-11-13 was not found.** Those builds, `ff6e654` and `fa2fa6b`, ran
  at ~6 KB/s (inferred from timing) with ~1.2 s stalls. Which change since removed the stop was not
  identified.
- **ATT error 0x0E is BlueZ's answer to a write pending when the link drops.** The issue saw it, and
  the cut-link run below saw it again.

Not tested in these runs: other LE devices sharing the PC's adapter during an update, and
centrals other than the bench PC, such as a Pi at 1M and 48.75 ms, or a phone. Floods beside two
other LE devices found the host stop later the same day ([The host stop,
found](#the-host-stop-found)).

### `flash-bridge --link bluetooth`

The image is 1,009,696 bytes, the instrumented build. The times come from the command's own lines.

| Case | Runs | Sending | Install to `installed and confirmed` | Whole command | Result |
| --- | --- | --- | --- | --- | --- |
| P4 image changed, C6 unchanged | 4 | 66.0–66.2 s (15.3 KB/s) | 8.0–8.4 s | 75.3–76.1 s | All installed and confirmed |
| Both chips changed | 5 (4 timed) | 66.2–66.5 s | 13.0–13.3 s | 80.4–81.0 s | All installed and confirmed |
| Over USB, P4 only | 1 | 12.8 s (79 KB/s) | — | 18.4 s | Installed and confirmed |
| Over USB, both chips | 1 | 12.9 s | — | 19.8 s | Installed and confirmed |

- **The install-to-confirmed time includes the command's 5 s wait** for the link to stay quiet.
- **The link dropped once per update.** With both chips changed, the central's first reconnect,
  7.3 to 8.3 s after the install, already found the updated C6. So the C6's update had ended before
  any host frame could confirm the P4.
- **Every image was confirmed on its first trial boot,** well inside the 30 s trial watchdog
  (inferred from the single reconnect and the times; the trial sector was not read). The watchdog
  stays as it is.
- **A last update over Bluetooth put the production image of `746c919` on both chips:** 62.7 s
  sending, 13.2 s to confirmed. `flash-bridge` over USB then reported `already runs this image`.
- **A forced rollback was reported.** A P4 image built to panic right after its boot check ended
  the command with `D0:1C did not boot the new image; rolled back`, 6.8 s after the install.
  `wartui status --link bluetooth` answered afterwards, and no USB was used.
- **A cut link was reported.** The PC's adapter was powered off 30 s into a send, at 43%. The
  command ended with `the link dropped before the update finished (write failed: Operation failed
  with ATT error: 0x0e); the bridge keeps its image, and flash-bridge can be run again`. The rerun
  installed and confirmed.

### `ota --link bluetooth`

Ten nodes, forced, to node image 0.5.2 (888 KB).

- **The cache upload to the bridge took 52.4 s,** 16.9 KB/s.
- **The ten nodes then took 29 s,** each at about 35 KB/s as the command reports it. That is the
  node bus's rate, not the link's (inferred): the chunk requests are small frames the transport
  packs into write groups.
- **All ten ended `done 0.5.2 (confirmed)`.** The whole command took 92 s, the 6 s listen and a 3 s
  build included. No update record was dropped, and the run's log holds no warning.

Not run: a node rollback over Bluetooth. A node's session is the same over either link; the USB
runs are in [`ota-findings.md`](ota-findings.md#failures).

### Seen, not explained

Neither is specific to Bluetooth.

- **While the P4 writes flash, its log alternates** `no GPS receiver answered on the I2C bus` and
  `the GPS receiver answered`, about once a second: 68 lines in a 66 s send over Bluetooth, 5 in a
  13 s send over USB.
- **After one of the ten updates over Bluetooth, the P4 found every node slot off.** Its boot line
  read `at boot 10 powered, 0 already on`. Every other boot read `0 powered, 10 already on`.

### After a review's fixes

A review of the branch gave ten findings, and seven were fixed: a paced sender that waited forever
on a link taking no commands, a ctrl-c that did nothing while `flash-bridge` built or downloaded
its image, `ota` over Bluetooth taking whichever bridge a scan heard, and four smaller ones. The
re-check ran the same day on D0:1C, with the fixes on the host:

- **`flash-bridge --link bluetooth`, both chips changed:** installed and confirmed, 66.1 s sending
  and 80.6 s in all.
- **`ota --link bluetooth`, ten nodes, forced:** the cache upload took 52.0 s, all ten ended
  `done 0.5.2 (confirmed)`, the whole command took 88 s, and the run's log holds no warning.
- **`ota --link bluetooth` with no bridge named or remembered over Bluetooth** was refused before
  any link opened.
- **The production image went back on both chips over Bluetooth,** and `flash-bridge` over USB
  reported `already runs this image`.

### The host stop, found

Measured on 2026-11-15 on D0:1C with its ten nodes, from the bench PC with a Bluetooth mouse and
one other LE device connected to its adapter: MTU 247, 2M PHY, a 30 ms interval. The C6 ran a
throwaway instrumented build, with counters in a private copy of esp-radio's receive path. Its
stock variant copies only the first buffer of a chain, as esp-radio does; the other copies every
buffer. The floods are the throwaway command above, into the P4's update receiver.

| Run | The C6's receive path | Write value | Result |
| --- | --- | --- | --- |
| 3 floods of 1.5 MB, fleet scanning | Stock: the first buffer only | 244 bytes | 2 floods verified, at 10.1 and 10.2 KB/s. The host stopped 11 s into the third, 308 s in, and the PC saw `write failed: Operation failed with ATT error: 0x0e` |
| Sweep, 500 KB | Copies every buffer | 170 to 200 bytes, in steps of 1 | Verified, no failed read. 463 packets arrived chained, every one 193 to 198 bytes with its ACL header, and 190 bytes in the first buffer. Packets of 181 to 192 and 199 to 211 bytes did not chain |
| Soak, 4 floods of 1.5 MB, 9 min | Copies every buffer | 179 bytes | All verified, 11.1 KB/s, no link drop. 36,731 packets, none chained |
| Contrast, about 6 min | Copies every buffer | 244 bytes | 4 packets chained in about 22,400, at 126, 178, 193 and 247 s. None failed, since this build copies the chain |

- **The stop is a chained packet cut short.** The C6's kept panic line names one failed read: an
  ACL packet (kind `0x02`) queued as 191 bytes, with the header `02 00 20 be 00 f7 00 04 00 52 0c
  00`, so ACL data length 190, L2CAP length 247, CID 4, an ATT Write Command. The counters give
  one chained packet: 194 bytes in all, 4 of ACL header and 190 of data, in 2 buffers from the
  pool whose blocks hold 240 bytes, 190 of them in the first. The packet reached bt-hci short of
  the length its header gives, the read failed, esp-radio mapped that to `Controller(Unknown)`,
  TrouBLE's host ended, and the C6 restarted. This is the host stop of issue #314.
- **A packet of 193 to 198 bytes chains, its 4-byte ACL header included:** 189 to 194 bytes of ACL
  data. A write of N value bytes is an ATT PDU of N+3 bytes and an L2CAP PDU of N+7, so an
  unfragmented write of 182 to 187 value bytes chains by itself, on any adapter. A write group's
  last piece can be any length, so the transport could send one with nothing else on the adapter.
- **A full 244-byte write chains only when the central fragments it.** Its L2CAP PDU of 251 bytes
  normally arrives as one 255-byte packet, from the larger pool, and does not chain. A central
  whose controller shares air time with other connections sometimes sends it as a fragment of 190
  bytes of data and a remainder, and that fragment chains.

**The fix caps every write toward the bridge at 179 value bytes**
(`wartui_proto::ble::DOWN_VALUE_MAX`), in the host transport. The L2CAP PDU is then at most 186
bytes, so no packet, and no fragment of one, passes 190 bytes with its ACL header, whatever the
central's controller does. The first group's leading `0x00` rides in its first piece, so the
largest host frame and that byte fit the group's 7 writes. No dependency is patched. The cap
binds only wartui's transport: [the C6's MTU](#the-c6-serves-an-att-mtu-of-182) binds every
central.

The same two devices stayed on the PC's adapter for the runs with the cap:

| Run | C6 receive path | Result |
| --- | --- | --- |
| Soak, 4 x 1.5 MB into the P4's update receiver, fleet scanning, 179-byte writes | Stock | All verified, 10.2 KB/s, no link drop in 587 s. 36,614 packets, none chained, the largest 190 bytes |
| `flash-bridge --link bluetooth`, both chips changed, host with the cap | Stock, then the production image it installed | Installed and confirmed: 93.3 s sending, 13.7 s from the install |
| `ota --link bluetooth`, ten nodes, forced, host with the cap | Production | The cache upload took 81.4 s (10.9 KB/s), all ten ended `done 0.5.2 (confirmed)`, 140 s in all, and the run's log holds no warning |

- **One run each way, under the same load beside the same devices:** the stock path died at 308 s
  with 244-byte writes and ran 587 s with the cap. The clean run is weak evidence alone, since
  the uncapped contrast run chained only 4 packets in about 6 min. The cap rests on the lengths:
  every chained packet seen was 193 to 198 bytes, and a capped write sends none over 190.
- **The cap costs no speed there:** 10.2 KB/s against 10.1 and 10.2 KB/s uncapped.
- **A shared adapter is slower.** The update sent at about 10.8 KB/s beside the two devices, against
  15.3 KB/s alone.
- `flash-bridge` over USB then reported `already runs this image`, and the bridge held no panic
  line.

A second sweep the same day covered the lengths under the first, with one other LE device on the
PC's adapter and the C6 on the build that copies every buffer:

| Run | Write value | Result |
| --- | --- | --- |
| Sweep, 741 KB, every write a request | 1 to 181 bytes, in steps of 1 | Verified, no failed read, no link drop. 24,474 packets, the largest 192 bytes, none chained |

- **No packet of 12 to 192 bytes chains.** With the first sweep, every length from 12 to 211 bytes
  is measured, and only 193 to 198 chain. A capped write and its fragments stay at or under 190.
- Each length carried one 4 KB sector: about 24 writes at 181 bytes, and more at each shorter
  length (inferred from the sizes). In the first sweep most packets of 193 to 198 bytes chained,
  463 of ~630 (inferred from the packet count), so a window as wide would show in one sector.

### The C6 serves an ATT MTU of 182

The host-side cap leaves every other central free to send a long write, so the C6 now serves an
ATT MTU of 182 (`wartui_proto::ble::ATT_MTU`), TrouBLE's packet pool at 186 bytes. A conforming
central then sends no ATT PDU over 182 bytes, so no write over 179 value bytes and no packet over
190. A notification toward the host carries 179 bytes where it carried 244.

Measured on 2026-11-15 on D0:1C with its ten nodes, one other LE device on the PC's adapter, 2M
PHY, a 30 ms interval. The cost toward the host ran on the load branch (`bench/ble-load`,
`e59424a`), built at each MTU in turn: 247, 182, 182, 247. BlueZ reported an MTU of 182 on Up
and Down in the 182 arm.

| ATT MTU | `saturate`, 60 s, read by the host | Credited by the C6 |
| --- | --- | --- |
| 247 | 102.9 and 103.1 KB/s | 104.8 and 105.0 KB/s |
| 182 | 98.5 and 99.2 KB/s | 100.4 and 101.1 KB/s |

- **MTU 182 costs about 4% of the saturated rate toward the host.** The notification is 27%
  shorter, so the PC's link is not bound by the notification's size.
- Each run's missing records were no more than the P4's evictions: 3, 1, 2 and 0 missing against
  3, 1, 3 and 0 evicted.

The production image then ran on the same bench:

| Run | Result |
| --- | --- |
| `flash-bridge --link bluetooth`, both chips, from the MTU 247 load build to the production image | Installed and confirmed: 62.4 s sending, 77.0 s in all. The host's own cap carried it, since that C6 served 247 |
| `status --link bluetooth` | `host  bluetooth, encrypted and bonded, MTU 182, 2M PHY, 30 ms interval` |
| `ota --link bluetooth`, ten nodes, forced | The cache upload took 52.0 s (17.1 KB/s), all ten ended `done 0.5.2 (confirmed)`, 91 s in all, and the run's log holds no warning |

- `flash-bridge` over USB then reported `already runs this image`, and the bridge held no panic
  line.
- **Not run:** a phone on the MTU of 182, and a central that sends more than the MTU it agreed.

## macOS

Measured on 2026-11-20 on D0:1C with its ten nodes, from a Mac: macOS 27.2.1, its built-in Apple
controller (BCM4388), and no other device connected to it. The firmware is `main` at `75ce526`
on both chips and every node (0.6.0). The host is `75ce526`, and from the resume checks on, with
the reconnect fix below. CoreBluetooth chose MTU 182, the 2M PHY and a 30 ms interval on every
connection. No load branch ran, so the saturated rate was not measured.

| Check | Result |
| --- | --- |
| No bond, window closed | Refused at 10 s: `refused this PC over Bluetooth because they are not paired`. One `unbonded_kicks`, reason `0x16`, no failed pairing |
| First pairing, `bluetooth pair` over USB then `status --link bluetooth` | macOS raised its own dialog. The first command gave up at 10 s, `did not answer the subscription`; the pairing completed, and the next was served, 2 bonds stored |
| `status --link bluetooth` on the bond | 1.6 to 2.5 s in all, about 1 s from connecting to subscribed |
| 10-minute scan, ten nodes | 1,414 frames received by the bridge and 1,414 read by the host. 0 batches lost, 119 or 120 heartbeats from every node, one connection |
| Lag behind the bridge | 25 ms at the median, 39 ms at p95, 77 ms at worst |
| `wartui ports` during a run | Nothing lost or logged |
| USB takeover (`status --link usb`) | Served again 15.00 s after USB let go |
| `reset --link usb` | Subscribed 2.0 s after the link dropped, served 15.0 s after it |
| The Mac's Bluetooth off for about 4 s, then on | Served about 10 s after it went off, with no prompt |
| `flash-bridge --link bluetooth`, both chips changed | Installed and confirmed: about 80 s sending (12.6 KB/s), 13.1 s from the install, 95 s in all. The C6's update is inferred from that time, which is Linux's for both chips |
| `flash-bridge --link bluetooth`, back to `main` | Installed and confirmed: about 83 s sending, 97 s in all. Over USB then `already runs this image` |
| `ota --link bluetooth`, ten nodes, forced | Cache upload 71.2 s (12.5 KB/s), all ten `done 0.6.0 (confirmed)`, 162 s in all |
| `bluetooth forget --link usb`, then pair again | Bonds cleared. The Mac, still bonded, could not connect; once forgotten in System Settings it paired as the first time did |

### A run that lost its link never got it back

**On the host as built, a run whose link dropped stayed down for good.** After `wartui reset`, the
link dropped as the C6 restarted, and every try after it failed at once with `could not connect to
D0:1C: Runtime Error: Peripheral no longer available`, backing off to every 10 s while the bridge
advertised. One run stayed so for 5.5 min, to its end. Linux reconnects in 2.1 s.

- **btleplug's CoreBluetooth backend keeps a dead handle.** A disconnect drops the peripheral from
  the backend's own table, and with it the task that carries its notifications, but leaves it in
  the adapter's (`btleplug 0.13.5`, `corebluetooth/internal.rs`, `on_peripheral_disconnect`;
  `corebluetooth/adapter.rs`, the `DeviceDisconnected` arm). The adapter then never builds another
  for that identifier. So `peripheral(&id)`, `peripherals()` and a rescan all hand back the dead
  handle: its connect fails, or it connects and its notifications go nowhere.
- **The transport kept that handle for the run's life.** A run settled on the peripheral that
  announced a bridge, and reused it on every try. BlueZ keeps its device object across a
  disconnect, so Linux never saw it.
- **The fix takes a fresh handle each try on macOS**, by its identifier
  (`retrieve_peripherals`), which builds a live one for a peripheral the backend dropped. The run
  now settles on the bridge's address, and checks each connection's first `Ready` against it.
  Linux is unchanged. With it, the reset, the takeover and Bluetooth off and on above all
  recovered.
- **A try that scans clears btleplug's tables first.** A rescan can re-add a dropped peripheral
  to the backend's table, and a retrieval after that returns nothing, so that path could still
  meet the dead handle; whether it does is a race (from reading btleplug). A link that dropped
  before its `Ready`, with USB holding the bridge and a `reset` over USB, recovered on the host
  before the clear and after it: served 15.4 s after USB let go, both times.
- **The resume run lost 206 frames, 16 sighting batches among them,** across its three outages:
  15 s of takeover, a reset with 15 s of USB holding the bridge, and about 10 s with Bluetooth off.

### What else differs from Linux

- **The first connection inside the window always fails.** CoreBluetooth's subscription waits on
  its pairing dialog, whose button is Connect, and the transport gives up at its 10 s
  (`CONNECT_WITHIN`). The pairing completes regardless. It happened on both first pairings. The
  C6 gives an unbonded central 30 s while the window is open, so a longer subscription deadline
  inside the window fits a click into one command: see
  [The pairing dialog inside one command](#the-pairing-dialog-inside-one-command).
- **A Mac holding a bond the bridge dropped cannot connect,** and reads `could not connect to
  D0:1C in 10s; check the bridge is powered and in range`. On Linux `pair` reads BlueZ and names
  that case; on macOS nothing does.
- **A command exits at once.** No 2 s disconnect at exit, as BlueZ has.
- **Sending toward the bridge is about 20% slower:** 12.5 to 12.6 KB/s for both updates and the
  cache, against 15.3 to 17.1 KB/s from the bench PC on the same MTU, PHY and interval. Where
  CoreBluetooth spends the time was not measured.
- **Every disconnect logs `Shouldn't get anything but Ok!` at error level,** from btleplug, which
  gets a reply it does not expect to a disconnect and carries on (`corebluetooth/peripheral.rs`,
  `disconnect`). It is harmless.
- **Two wartui processes on one Mac share one connection.** A `status --link bluetooth` started
  beside a run went through the run's connection, and was served in 10 s. A `reset` the same way
  reset the bridge, on the host before the fix, and stranded both.

### The pairing dialog inside one command

Measured later on 2026-11-20, on the same Mac and bridge, with the host giving the subscription
30 s off Linux (`PAIR_WITHIN`) and announcing the wait 3 s in (`DisconnectCause::AwaitingPairing`).

| Check | Result |
| --- | --- |
| `status --link bluetooth` on the bond | Served with no notice, 4.0 s in all from a debug build |
| First pairing: `bluetooth forget --link usb`, the bridge forgotten in System Settings, `bluetooth pair` over USB, then one `status --link bluetooth` | The notice 6.4 s into the command, 4.4 s after connecting. Connect clicked on purpose late. Subscribed 22.2 s after connecting and served at once, `encrypted and bonded`, `pairings 1`, `failed_pairings 0`. One command, 23.6 s in all |

- **The click landed past the 10 s the transport gave before,** and inside the C6's 30 s.
- **Connecting to the subscription's start took about 1.4 s,** so the notice came 4.4 s after
  connecting.
- **A dialog left unclicked was not measured.**

## What this settles

- The production link passes every check the PC can run, on D0:1C with its ten nodes.
- The Pixel and the iPad pair from their own prompt in the window, and drain at the spike's rates
  or above.
- The bridge drops a central it holds no bond for outside the window, a phone as well as the PC.
- A bonded central that never subscribes is dropped after 30 s, and one that subscribes and waits
  is not.
- A lost P4→C6 packet costs the frames it cut and nothing else.
- One host at a time holds: a port probe takes nothing, a USB command takes the bridge, and the
  Bluetooth host gets it back.
- The C6 does not ask for a connection interval by default.
- A two-hour drive from a Pi on the 1M PHY loses nothing, with the bridge's GPS feeding the
  positions over the same link.
- Updates run over Bluetooth from the bench PC, at about 15 to 17 KB/s toward the bridge. A bridge
  update takes about 75 to 81 s, against 18 to 20 s over USB.
- `flash-bridge` over Bluetooth reports a rollback and a cut link, and the bridge keeps a working
  image through both.
- What stopped TrouBLE's host: an ACL packet of 193 to 198 bytes reaches the C6 as a chain, and
  esp-radio passes on only its first buffer. The C6 serves an ATT MTU of 182, so no packet a
  conforming central sends reaches that size, and wartui caps its own writes at 179 value bytes.
- An ATT MTU of 182 costs about 4% of the saturated rate toward the bench PC.
- A Mac pairs from its own dialog, scans ten nodes for 10 minutes with nothing lost, recovers
  from a takeover, a reset and its own Bluetooth going off, and runs both updates, with the
  reconnect fix.

## What this does not settle

- **A phone through a disconnect, a bridge reset and a C6 reset.** Each phone ran only the
  pairing, the load and the USB takeovers.
- **A Down write that starts nothing.** The P4 ignores a frame that does not decode, and a write
  from a central that does not hold the bridge. It counts neither, so the host gets no sign of it.
- **A bond that reaches the C6 while it advertises, for a central on a resolvable private
  address.** It needs a phone and a bench build that delays the P4's bonds.
- **A lost bond message recovered by its repeat.** It needs a bench build that discards one.
- **A second connection on the PC's adapter** cut the saturated rate to 16 KB/s. A real fleet
  sends under 3 KB/s, and no run measured one beside a mouse.
- **Saturated USB through two hubs at 12 Mbit/s** loses records on every image. A real fleet
  sends under 3 KB/s.
- **Updates from another central,** a phone or the Pi. Only the bench PC ran them.
- **A phone on the ATT MTU of 182.** Each phone's rates were measured at 247.
- **A node rollback over Bluetooth.** It was not run.
- **Why the saturated rate moves between two levels.** It is not the build: both images held the
  upper level through a later session of runs.
- **The saturated rate on the 1M PHY.** The drive needed a few KB/s of it.
- **Why a first connection sometimes times out.**
- **macOS's saturated rate,** and a phone or a second LE device beside the Mac.
- **Why sending from a Mac is about 20% slower** than from the bench PC.
