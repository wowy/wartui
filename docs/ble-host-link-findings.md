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

No phone ran. The iPad and the Pixel are listed under
[What this does not settle](#what-this-does-not-settle).

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

## What this settles

- The production link passes every check the PC can run, on D0:1C with its ten nodes.
- A lost P4→C6 packet costs the frames it cut and nothing else.
- One host at a time holds: a port probe takes nothing, a USB command takes the bridge, and the
  Bluetooth host gets it back.
- The C6 does not ask for a connection interval by default.

## What this does not settle

- **The iPad and the Pixel.** Neither ran against the production link. Each holds a bond from the
  spike, which the bridge no longer has, so each must forget the bridge and pair again.
- **A central the bridge holds no bond for, other than this PC.** The check above is a PC that
  dropped its own bond.
- **Updates over Bluetooth.** `flash-bridge` and `ota` stay on USB (issue #314): TrouBLE's host
  stops under sustained writes toward the bridge.
- **Why the saturated rate moves between two levels.**
- **Why a first connection sometimes times out.**
- **macOS.** The transport builds there and is untested.
