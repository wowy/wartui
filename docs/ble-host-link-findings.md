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
over Bluetooth ran the same day too: see [Updates over Bluetooth](#updates-over-bluetooth).

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

Not tested: other LE devices sharing the PC's adapter during an update, and centrals other than
the bench PC, such as a Pi at 1M and 48.75 ms, or a phone.

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
- **What stopped TrouBLE's host on 2026-11-13.** The 2026-11-15 floods did not reproduce it, and
  no change since is known to have removed it.
- **Updates beside other LE devices on the PC's adapter, or from another central.** Only the bench
  PC, alone on its adapter, ran them.
- **A node rollback over Bluetooth.** It was not run.
- **Why the saturated rate moves between two levels.** It is not the build: both images held the
  upper level through a later session of runs.
- **The saturated rate on the 1M PHY.** The drive needed a few KB/s of it.
- **Why a first connection sometimes times out.**
- **macOS.** The transport builds there and is untested.
