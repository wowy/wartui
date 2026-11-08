# Node bus — first light at 1 MHz

Measured on 2026-11-05 on Linux (Fedora 44). The bridge was a Waveshare ESP32-P4-WIFI6 (`D0:1C`).
There were two XIAO ESP32-C5 nodes, wired straight to its header with no carrier board, one per
bus (`docs/node-bus.md` § "Wiring two nodes without a carrier"):

| Slot | Node | Jumpers |
| --- | --- | --- |
| A1 | `0D:20` | short |
| B1 | `1A:50` | 12" (30 cm) |

The firmware was from branch `feat/node-bus-bringup` (dc6da3b). The bus ran at 1 MHz in mode 0,
with fixed 1,040-byte transfers. Each node sent a copy of every heartbeat over the bus, and ESP-NOW
carried the fleet as before. The run was `wartui run` for 10.5 minutes.

## No errors at either end

| | A1 (`0D:20`) | B1 (`1A:50`) |
| --- | --- | --- |
| Good transfers at the P4 | 2,923 | 2,919 |
| Good transfers at the node, since its boot | 3,008 | 3,042 |
| Bad magic, CRC, version or length, either end | 0 | 0 |
| Seq gaps, either direction | 0 | 0 |
| P4 retries | 0 | 0 |
| Node re-arm misses (`early`) and arm failures | 0 | 0 |
| Heartbeat records at the P4 | 159 | 159 |
| P4 IRQ-to-transfer latency, mean of the last window / max over the run | 10.3 / 18.0 ms | 9.4 / 9.7 ms |

- **Bus B's 30 cm jumpers made no difference** at 1 MHz.
- **The node re-arms in time.** Its CS-edge interrupt collects each transfer and arms the next.
  The 1 ms gap the P4 leaves after each transfer to a slot was always enough: no transfer arrived
  at an unarmed node, and none ended before its DMA reported done.
- **The IRQ latency is about one transfer.** A transfer takes 8.3 ms at 1 MHz. When a heartbeat
  is queued, the node already has an idle transfer armed, so the P4 clocks that one first and
  collects the record on its next pass. The 18 ms maximum is two passes. At 4 MHz, a pass is
  about 2 ms.
- **`replaced` did not grow during the run.** It counts heartbeat copies replaced by a newer one
  before the P4 clocked them. It already stood at 15 and 6 at the first reading, 40 s in, and
  stayed there. The nodes were flashed and running before the P4 was, so those copies most
  likely queued while nothing polled.

## The DMA is done before the edge's handler looks

A later build (db7155d) bins each CS edge by how long the handler waits for the SPI driver's DMA to
report the transfer done. Its limit is 50 µs. Both nodes were rebooted under a P4 that was
already polling, and ran for 3 minutes:

| | Done on the first check | Spun | Longest time from edge to first check |
| --- | --- | --- | --- |
| `0D:20` | 941 of 941 | 0 | 41 µs |
| `1A:50` | 990 of 990 | 0 | 45 µs |

- The wait never ran. So the 50 µs limit is a guard, not a cost paid per transfer.
- The 41–45 µs is the handler's own time to reach and make its first check, with interrupts
  masked. The cause isn't known. Flash-cache misses in a handler that runs from flash are a
  guess that was not tested.
- In an earlier run, one `early` and one seq gap per node appeared. They came from the P4
  rebooting mid-transfer during its own update, and did not recur once the nodes rebooted
  under a running P4.

## ESP-NOW carried on alongside

The capture recorded 512 observations and 255 heartbeats, with no batch gaps. The engine saw no
reboot from either node. So serving the SPI driver from a GPIO interrupt did not disturb the
promiscuous callback, the Bluetooth scan, or the ESP-NOW sends.

`1A:50` joined with reset cause `watchdog` and `0D:20` with `external`. Both causes date from the
reset after `flash-fleet`, not from this run.

## Still open

- A shared bus. Several nodes on one CS-selected bus, with MISO tri-stated by the node while
  its CS is high, waits for the carrier board.
- Speed. This run used 1 MHz. The carrier target is 4 MHz.
- Load. Only heartbeats crossed the bus. Sighting batches cross it from the wire v1 cut onward.

## Wire v1: the bus alone, 2026-11-05

Branch `feat/wire-v1` takes the nodes off ESP-NOW. Heartbeats, sightings and assignments cross
the bus as records, each data transfer is acknowledged, and a sender waits for its ack before
sending more. Same boards and wiring as above. The P4 was wired-flashed over its CH343.

**The first build (00a8ab2) dropped transfers at the node.** Its CS handler kept two finished
transfers for the main loop, and the main loop reads the bus only between 125 ms dwells. While
the P4 waits for an ack it clocks the slot every 25 ms, so idle transfers crowded out data:

| 10 minutes | `0D:20` | `1A:50` |
| --- | --- | --- |
| Transfers dropped unread | 125 | 153 |
| Assignment delivery, host send to `Delivered` | 60 ms | 60 ms, then 1,094 ms |

The 1,094 ms delivery is one dropped data transfer and the P4's resend after its 1 s ack
timeout.

**The fix (d97134b) checks each transfer in the handler.** An idle transfer gives its buffer
back at once. A data transfer queues, and the next transfer armed acknowledges it. A second
10-minute run:

| | `0D:20` (A1) | `1A:50` (B1) |
| --- | --- | --- |
| Transfers at the node | 2,999 | 3,090 |
| Bad, dropped, gaps, resent or undelivered | 0 | 0 |
| Assignment delivery | 35, 35 ms | 35 ms |
| Longest CS handler run | 236 µs | 234 µs |
| Heartbeats recorded | 120 | 120 |

The capture recorded 407 observations, with no batch gaps.

- The handler now runs from RAM and makes two CRC passes per edge, one checking and one
  sealing. That costs up to 236 µs with interrupts masked, at roughly 4–10 edges a second. The
  sightings kept arriving, but whether the radio loses frames to that window wasn't measured.
  The CRC table is still in flash.
- The P4's error counters rose only while the nodes were reflashing.

**After the review fixes**, a 7-minute run on the same bench: both nodes 0 bad, gaps, resent or
undelivered across 2,201 and 2,260 transfers, three assignments delivered in 35 ms, 421
observations and no batch gaps. The P4 counted no seq gap when the nodes rejoined after their
reflash, where earlier runs counted one; its 10 magic misses and 2 retries per slot date from
that reflash.

## Node updates over the bus, 2026-11-06

`wartui ota --force` on the same bench at 1 MHz. Each node was reinstalled with its own version,
0.3.1, which is the hardest case to confirm: the version can't tell the new image from the old.

| | 0D:20, bus A | 1A:50, bus B |
| --- | --- | --- |
| Image | 905 KB | 905 KB |
| Rate while sending | 24 KB/s | 24 KB/s |
| Outcome | confirmed on the new image | confirmed on the new image |

Both ran first time, with no restart of the session. The timed run, 1A:50's, took 45 s from start to
confirmed, its 6 s listen and the image build included.

- **The rate is one round trip per chunk.** One chunk is in flight at a time, and each waits
  for the node's ack, which the P4 fetches on its 25 ms ack poll, then for `Delivered` to reach the
  host. That is about 42 ms per KB. Pipelining chunks, or a faster clock, would raise it.
- **Rollback.** An update cancelled at `rebooting` sends no confirm. The node booted the new image
  as `trial boot 1 of 3`. Two resets through its USB counted boots 2 and 3, and the third rolled it
  back to the old slot and rebooted. It then heartbeated off trial, with nothing sent to it.

## On the carrier-10, 2026-11-06

The same P4 (`D0:1C`), seated on a 10-node carrier, with its CH343 as the host link. The P4 ran
branch `feat/carrier-board` (ed43819), and the two nodes ran 1c614db's node firmware, both at
1 MHz. A GPS receiver sat on I2C0 with the carrier's I/O expander. The nodes were the same two
XIAO C5s, powered by their sockets alone.

**Detection and boot.** `wartui status` named the board `carrier-10`, 10 node slots, N1 to N10.

| Boot | The P4's carrier line |
| --- | --- |
| Carrier powered on | `10 powered now, 0 already on` |
| P4 reset alone, through the CH343's EN line | `0 powered now, 10 already on` |

After the P4-only reset, the expander kept its outputs and no socket was switched off. The P4
reports that reset as `PowerOn`, because the CH343 pulls the chip's EN pin.

**Every socket.** The two nodes moved through the sockets in pairs, one per bus: N1 and N6, N2
and N7, and so on to N5 and N10. In each socket the node heartbeated, its IRQ was served, and
`wartui power-cycle` rebooted it.

| | Bus A, N1–N5 (`0D:20`) | Bus B, N6–N10 (`1A:50`) |
| --- | --- | --- |
| Heartbeats through the socket's CS | all five | all five |
| IRQ-to-transfer latency, mean | 8.8–9.6 ms | 9.4–9.7 ms, 12.4 ms over N6's first 33 |
| After `power-cycle` | heartbeat `#1`, reset `power_on` | heartbeat `#1`, reset `power_on` |
| The other bus's node during the cycle | ran on unbroken | ran on unbroken |
| CRC errors, by socket | N3 1, N4 4, others 0 | N8 5, others 0 |
| Stuck IRQ | 1, at N3's move | 0 |

- **The 1 s off time is enough.** Every cycled node came back with reset cause `power_on`, not
  a brownout.
- **A power-cycle costs two retries.** The P4 counts them while the node is still present and
  silent, before it marks the node gone. A node left alone in its socket counted none.
- **The CRC errors came while nodes were moved with the carrier powered.** The P4's uptime ran
  unbroken from the move to N3 and N8 through N10. Each socket's count was already standing at
  the first reading after a move, and none grew afterwards, over at least 100 good transfers per
  socket and through its power-cycle. N5 and N10, also moved hot, counted none.
- **The GPS shares I2C0 with the expander.** The receiver streamed NMEA, about 400 bytes a
  second, through the power-cycles of N1 and N6, and got a fix.

**A 10-minute capture with N1 and N6 undisturbed:**

| | N1 (`0D:20`) | N6 (`1A:50`) |
| --- | --- | --- |
| Heartbeats recorded | 120 | 120 |
| CRC, version, length or seq gap | 0 | 0 |
| Retries during the run | 0 | 0 |
| Stuck IRQ | 0 | 0 |
| IRQ-to-transfer latency, mean / max | 9.6 / 18.1 ms | 9.7 / 18.2 ms |

The capture recorded 380 observations from 116 networks. There were no batch gaps, and every
observation had a GPS position. All three assignments were delivered in 34–35 ms. The bridge
dropped no frames, and the host stored all of them.

### Still open on the carrier

- **Several nodes on one bus.** Each bus held one node at a time, so no two nodes shared MISO.
- **4 MHz.**
- **The 14-node board's seventh CS on each bus** (GPIO25 and GPIO24).

## A full carrier-10, 2026-11-06

The same P4 and carrier, with a node in every socket, five per bus. The bus ran at 1 MHz.

| Bus A | N1 `04:E4` | N2 `00:40` | N3 `97:78` | N4 `0A:28` | N5 `0D:20` |
| --- | --- | --- | --- | --- | --- |
| **Bus B** | **N6 `7E:3C`** | **N7 `EF:54`** | **N8 `F7:30`** | **N9 `E5:60`** | **N10 `1A:50`** |

Every node heartbeated over the bus, with no CRC, version, length or seq-gap error on any slot.
`wartui power-cycle` rebooted each of the ten with reset cause `power_on`, at two retries apiece.

**Five nodes on one IRQ line cost a pass per record.** An IRQ pass clocks every present slot
once, about 46 ms with five at 1 MHz. A record waits one or two passes:

| IRQ-to-transfer latency, mean / max | Bus A | Bus B |
| --- | --- | --- |
| One node, any socket | 8.8–9.6 / 9.6 ms | 9.4–9.7 / 9.7 ms |
| Five nodes, after a P4 reset (ed43819) | 34.5 / 69 ms | 36 / 69 ms |
| Five nodes, after a cold start (03eefed) | 31.8 / 52 ms | 34.4 / 52 ms |

### A stuck IRQ latch that could not clear (fixed in 03eefed)

On ed43819, bus A came up from three cold starts in a row with `irq_n=2 stuck=1`, and stayed
there for minutes. Its five nodes' heartbeats then arrived exactly 1,010 ms apart, the
timed schedule's grid, where IRQ service gives each node's own cadence. Bus B latched once on
each cold start and recovered.

- At a cold start, nodes assert IRQ before the P4 has marked them present, so two passes over
  the present slots bring nothing and the bus latches the line as stuck. That is intended.
- The latch cleared only on seeing the line deasserted. Served by the timed schedule, each
  node holds IRQ while its record waits for a poll, 250–500 ms of every second. Bus A's five
  heartbeats were spread across the second, at about .07, .40, .62, .80 and .89 s, so the
  line never deasserted.
- Power-cycling any one node, even one with no part in the fault, opened a gap and returned
  the bus to IRQ service.

03eefed also clears the latch when a slot becomes present, and retries IRQ service every
second. A cold start with all ten seated then counted no stuck line on either bus.

**A short on the carrier held bus A's IRQ too.** N4's D0, the node's IRQ output, had been
shorted to N3's 5 V pin. Once it was cleared, `0A:28` alone in N4 was served 22 IRQs for its
22 records at 9.1 / 9.6 ms, with no stuck line. So its D0 and the carrier's IRQ input for N4
both survived 5 V. The short does not explain the cold starts: bus A latched the same way after it
was cleared.

### The node supply lost while the P4 stays up (4ac6c60)

The P4 ran from its own USB-C, connected to the host, while the carrier's supply was switched
off for several seconds and back on. The host-side times are from `wartui sniff`:

| Time | The P4 |
| --- | --- |
| 21:32:59.54–.78 | all ten nodes gone, each after 5 transfers without magic |
| 21:33:00.05 | `every node dropped at once, so the node supply was likely lost`; every port low |
| 21:33:00–05 | scout N1, supply still out: no answer, switched off |
| ~21:33:05 | scout N6; the supply returns during its turn |
| 21:33:09.72 | `N6 appeared` |
| 21:33:11.05 | `N6 answered, so the node supply is back; 10 node slots powered again` |
| 21:33:11.53–.82 | the other nine appear |

The ten drops fell within 0.25 s of each other, well inside the 2 s window. The bridge dropped no
frame. Before 4ac6c60, the expander kept every port high through the loss, so all ten started
together when the supply returned.

### Still open on the full carrier

- A capture with all ten nodes assigned and recording.
- A power-cycle request while the P4 scouts. Host tests cover only the detection.
- 4 MHz, which cuts a five-node pass to about 12 ms.

### A P4 that starts before the node supply (e688afb)

The carrier's supply was switched off, the P4's USB-C unplugged and plugged back in, and the
supply switched on about 20 s later. Times are from the host's `--log-file`:

| Time (UTC) | The P4 |
| --- | --- |
| 04:22:56.19 | a power-on reset; `at boot 10 powered, 0 already on`, into the dead supply |
| 04:23:01.14 | `no node answered after power-up, so the node supply may be out`; every port low |
| 04:23:01–16 | scouts N1, N6 and N2, 5 s each, unanswered |
| 04:23:17.26 | `N7 appeared`: the supply returned during N7's turn |
| 04:23:18.16 | `N7 answered, so the node supply is back; 10 node slots powered again` |
| 04:23:19.25–.34 | the other nine appear |

Before e688afb, the boot stagger left every port high in the dead supply, and nothing
staggered the nodes when it returned.

### A power-cycle while the P4 scouts (8494180)

With the supply off and the P4 scouting, `wartui power-cycle --slot N3`:

| | e688afb | 8494180 |
| --- | --- | --- |
| The request ends the scouting | after the 0.9 s stagger | after the 0.9 s stagger |
| The cycle runs | after the 5 s check for a node | at once |
| `N3 power-cycled` at the CLI | never: it gave up at 5 s | 1.98 s after the request |
| Then | the check fails and scouting resumes | the check fails and scouting resumes |

After the e688afb run, the supply was switched on during the resumed scouting. N3 answered in
its turn, and the other nine followed 100 ms apart.

### Ten nodes recording, without antennas (4ac6c60)

A 10-minute capture with all ten nodes assigned, four or five channels each. The bus carried it
cleanly: no new CRC, version, length or seq-gap error across 1,200 counter readings, no new
stuck IRQ, no node lost, and none of the bridge's 1,234 frames dropped. Every assignment was
delivered, in 35–89 ms.

The nodes had no antennas fitted, so the capture says nothing about coverage: 17 networks
against 116 from two nodes with antennas in the same spot, heard 40–55 dB weaker.

### A power-cycle during the check for a node (3e144f3), 2026-11-07

With the supply off and the P4 scouting, `wartui power-cycle --slot N3`, then `--slot N5` as soon
as N3's answer came, which lands inside the 5 s check that follows N3's cycle:

| Time (UTC) | |
| --- | --- |
| 12:28:20.2 | N3's request ends the scouting |
| 12:28:21.10 | `a power-cycle request ended the wait; 10 node slots powered again` |
| ~12:28:22.1 | N3 cycled: 1.99 s at the CLI, as with 8494180 |
| ~12:28:22.2 | N5's request, inside the check |
| ~12:28:23.2 | N5 cycled: 1.08 s at the CLI |
| 12:28:28.19 | `no node answered after power-up`, and scouting resumes |

The check started again after N5's cycle. Before 3e144f3, N5 would have waited out the check
and been answered about 6 s after it was sent.

### Supply loss on 3e144f3, 2026-11-07

**All ten seated.** Recovery behaves as it did on 4ac6c60:

| Time (UTC) | |
| --- | --- |
| 12:31:02.86 | supply on; N1, the slot being scouted, appears |
| 12:31:03.76 | `N1 answered, so the node supply is back; 10 node slots powered again` |
| 12:31:04.86–.94 | the other nine appear |
| 12:31:23.57–.84 | supply off; all ten gone within 0.27 s, then `every node dropped at once` |
| 12:31:41.61 | supply on; N7 appears in its turn, after N1, N6 and N2 |
| 12:31:42.51–43.86 | `N7 answered`, and the other nine appear |

Across the drop and the return, N2 and N9 each counted one CRC error, every slot two retries,
and bus B two stuck IRQs. IRQ served both buses again afterwards, and no slot counted another
CRC, version, length or seq-gap error.

**Only N4 and N9 seated, a request during recovery.** The two dropped together at 12:35:24.08,
so recovery scouted N4 and N9 first. `wartui power-cycle --slot N1` at 12:36:10.6 ended the
scouting and was answered in 1.99 s. The check that followed found no node at about 12:36:17.6,
worked out from the answer and the 5 s check: the line came before the host's log was connected
again. Scouting then resumed in 5 s turns.

The supply was switched on during the second round. N4 appeared at 12:37:10.09, 2.5 s into the
round's first turn, about a node's boot time, and N9 followed at 12:37:11.95. With the slots that
held nodes carried through the request, a round starts N4, N9, N1, N6… Before 3e144f3 it started
at N1, and N4 would have waited for the round's seventh turn, about 30 s later.

### Ten nodes recording, with FPC antennas (3e144f3), 2026-11-07

Two captures with all ten nodes on FPC antennas: 10 minutes with N9's antenna missing, then 3
minutes with it fitted. Each node is compared by its RSSI on APs it heard while tuned to their
channel, against the median of the other nodes on the same APs. Most shared sightings are heard
from a neighbouring 2.4 GHz channel, which arrives weaker, so those are left out.

| | 10 min, N9 without antenna | 3 min, all fitted |
| --- | --- | --- |
| Networks | 131 | 110 |
| E5:60 in N9 | 5 sightings, 46 dB below the others | 53 sightings, 1 dB above |
| The other nine | 6 dB below to 9 dB above | 4 dB below to 4 dB above |
| Errors on the bus | none: no CRC, magic, version, length, seq-gap or retry, no stuck IRQ | none |
| Bridge frames dropped | 0 of 1,529 | 0 of 550 |
| Assignments delivered in | 35–122 ms | 35–113 ms |

Every heartbeat arrived in both. A missing antenna costs the same as in the antenna-less run,
40–55 dB. How much a node hears still depends on its channels: nearly every AP here is on 1, 6
or 11, so a node tuned elsewhere hears only their leakage.

## 4 MHz on the full carrier-10, 2026-11-07

`BUS_HZ` went from 1 MHz to 4 MHz (cb73e56), with nothing else changed: no other timing on the
P4 or the node was sized to a transfer, which now takes 2.09 ms instead of 8.35 ms. The nodes
take the P4's clock, so they kept their firmware. Two 10-minute captures with all ten nodes,
five on each bus: the 1 MHz one with N9's antenna missing, the 4 MHz one right after the
reflash with every antenna fitted.

| | 1 MHz (3e144f3) | 4 MHz (cb73e56) |
| --- | --- | --- |
| CRC, magic, version, length, seq-gap or retry errors | none | none |
| Stuck IRQ | none | none |
| IRQ-to-transfer latency, mean per 5 s window | 58–60 ms | 16–18 ms |
| IRQ-to-transfer latency, max since boot | 78 ms | 22 ms on bus A, 76 ms on bus B at boot |
| Assignments delivered in | 35–122 ms | 14–35 ms |
| Heartbeats | 120 per node | 120 per node |
| Bridge frames dropped | 0 of 1,529 | 0 of 1,592 |
| Networks | 131 | 143 |

The latency falls by about 3.5 times, not 4. Fitting the two means to a fixed part and a part
that scales with the clock leaves ~2 ms fixed (inferred from the two means): the 1 ms gap the
P4 waits once per pass, after its last transfer, and each transfer's setup. Each slot's transfer
count is about the same at both clocks, since polling and the nodes' records set it, not the
clock. The network counts don't compare: the 1 MHz run had N9 deaf.

The 14-node carrier, seven nodes on a bus, is still untested at either clock.

## Node updates over the 4 MHz bus, 2026-11-07

`wartui ota --force` to the same version (0.3.1), one node alone: 0D:20 in N5 on the
carrier-10, nine other nodes running.

| | 1 MHz bench (1 node) | 4 MHz carrier | 4 MHz carrier, ack fetched at once (ba62296) |
| --- | --- | --- | --- |
| Rate while sending | 24 KB/s | 21 KB/s | 32 KB/s |
| Whole run | 45 s | 53 s | 39 s |
| Outcome | confirmed | confirmed | confirmed |

The whole run includes the 6 s listen and an incremental image build.

- **The clock didn't help.** One chunk is in flight at a time, and each waited for the node's
  ack, which the P4 fetched on its 25 ms poll, then for `Delivered` to reach the host.
- **The P4 now fetches the ack right after the transfer that carried the chunk**, since the node
  stamps it on the very next transfer it arms. That alone took the rate from 21 to 32 KB/s. The
  25 ms poll stays for a transfer that brings no ack.
- The same build adds a node image cache in the P4's PSRAM, which this run didn't use: the host
  still sent every chunk itself.
- After the reflash, the buses were clean on all ten slots, with IRQ latency unchanged at about
  16 ms.

### PSRAM on the P4

A throwaway build, before the cache, measured the P4-WIFI6's PSRAM through esp-hal 1.2.2:

| | |
| --- | --- |
| Mapped | 32 MB at `0x48000000`; a marker at each MB read back correct, so no aliasing |
| Write 1 MB | 16.4 ms (64 MB/s) |
| Read 1 MB back | 21.7 ms (48 MB/s), no wrong words |
| Copy 1 KB into internal RAM | 23.5 µs mean, the same across all 32 MB; 0 of 1,000 wrong |
| SHA-256 of 1 MB, in software | 192 ms (5 MB/s) |

The buses, the GPS and the host link ran normally alongside it. Building a chunk from the cache
costs about 24 µs, against about 3 ms on the bus to carry it.

## Updating all ten nodes at once, 2026-11-07

`wartui ota --force --yes` to the same version (0.3.1), all ten carrier-10 nodes at once, 4 MHz.
The host uploads the image once into the P4's PSRAM cache and runs one session per node, each
with two chunks out, sent by reference from the cache.

| | 7e8be0d | ffde486 | f6f2d36 | fe1bde3 |
| --- | --- | --- | --- | --- |
| Cache upload, 884 KB | 2.6 s | 2.6 s | 2.6 s, beside the begins | 2.6 s, beside the begins |
| Confirmed | 8 of 10 | 10 of 10 | 10 of 10 | 10 of 10, twice |
| Rate per node while sending | ~15 KB/s | 13–23 KB/s | 10–11 KB/s | 23–24 KB/s |
| Whole run | 74 s | 80 s | 100 s | 50 s and 48 s |

The whole run includes the 6 s listen and an incremental image build.

### Nodes erasing flash went absent (fixed in ffde486)

On 7e8be0d, 0D:20 (N5) and 1A:50 (N10) failed with `node not answering`. Both went `gone after
5 transfers without magic`, then `appeared` 2 s later, every 2–3 s for 17 s, still making
progress between drops, so they weren't rebooting. Each drop answered their queued chunks
`NoNode`, and after enough failed sends their sessions gave up.

It wasn't those boards: alone, 04:E4 updated in 38 s and 0D:20 in 35 s, each with one drop, at
its reboot. But even alone, a node counted 160–170 magic failures during its update, about one
per 4 KB sector it writes. The reading, not yet measured directly: while a node erases a sector,
its flash operation holds interrupts, so it can't re-arm. In a fleet, the shared IRQ line is
nearly always asserted and each pass clocks every node, including one mid-erase, so five misses
in a row came fast enough to mark a node absent.

ffde486 makes absence need 1 s since the slot's last answer as well as five misses, and holds a
slot that missed for 10 ms before clocking it again. All ten then confirmed.

### Where the 80 s went (ffde486)

- **Before any chunk:** the 6 s listen, the 2.6 s upload, and each session waiting up to 5 s
  for its node's next heartbeat to send its `begin`.
- **Sending:** most nodes finished in 41–49 s. 00:40 (N2) and 97:78 (N3), both on bus A, ran at
  13 KB/s and finished at 67 s, so they set the time.
- **Magic failures:** 580–790 per node on bus A and 64–320 on bus B. Why bus A has more is open.
- **IRQ passes:** each node raises IRQ for each sector's status, and a pass clocks every node on
  the bus, so most of a pass's transfers carry nothing.

### Tuning that made it slower (f6f2d36)

f6f2d36 started streaming sooner (each `begin` sent at once to the slot heard in the listen, the
cache upload beside the begins), ended an IRQ pass at its first record, held a slot that missed
for 20 ms instead of 10, and added a second counter line per slot, `xfer`, counting transfers by
why they ran. The run took 100 s, every node at 10–11 KB/s. Two causes:

- **A rewind loop in the host's session.** With debug logging, the host sent 15,915 chunks for
  8,840 and drew 9,286 statuses for 2,220 sectors, 5,254 of them at mid-sector offsets; every
  send was `Delivered`. A node alone sent none of these (884 sends, 222 statuses). A resent chunk
  reached a node that had it, the node answered with a status naming where it stood, and that
  status arrived after the session had moved on. The session read it as a lost chunk and resent
  from it, and each resend drew more such statuses, every sector. On the node bus a `Delivered`
  chunk has reached the node, in order, so a status behind what was sent is never a loss.
- **The stuck-IRQ latch fired 27–32 times per bus** (3–8 on ffde486). A pass skipped a slot held
  after a miss, which may be the node asserting IRQ, so the pass came back empty and counted
  toward the latch. Each latch put the bus on polling for up to 1 s.

The `xfer` lines also showed IRQ passes are most of a node's traffic: about 4,000 pass transfers
per node, against about 1,000 carrying or fetching chunks.

### Under a minute (fe1bde3)

fe1bde3 stops the session moving back on a status: sending goes back only for an `Undelivered`
chunk, the status timeout, or `Idle`. A pass that skipped a held slot no longer counts toward
the stuck latch. With debug logging the host sent exactly 8,840 chunks and drew exactly 2,220
statuses; the latch fired 3–4 times per bus. Every node sent at 23–24 KB/s, and the whole run took
50 s, then 48 s without debug logging: about 9 s before the first chunk, about 39 s of sending,
then the reboots and confirms.

Nodes still miss transfers while they erase flash, 300–800 per node, but no longer go absent or
cost the update. Keeping chunks flowing across a sector's end is the next lever.

### After the review fixes (fc36e65)

A code review's fixes changed the P4 in two places: a missed transfer no longer clears the ack
the P4 owes a present node, and the cache's rules moved into `wartui-proto`, shared with the
sim. The host now ends a fleet at once when the P4 has lost its cached image, rather than
spending each session's retries. Rerun after flashing the P4:

| | fe1bde3 | fc36e65 |
| --- | --- | --- |
| Confirmed | 10 of 10 | 10 of 10 |
| Rate per node while sending | 23 KB/s | 22–23 KB/s |
| Stuck-IRQ latches per bus | 3–4 | 2–4 |
| Whole run | 48 s | 50 s |

The 2 s is within what two runs of the same build differed by (48 and 50 s on fe1bde3).

### Where the time goes now

Each node needed about 2.8 transfers per chunk in the 48 s run (from the `xfer` counters,
N1, N3, N6 and N8 agreeing), and each bus ran about 320 transfers a second against room for
about 340 (a transfer is ~2.9 ms with its setup): the buses are about 94% busy. So a sector's
stop costs a fleet little; fewer transfers per chunk is what would help. Roughly (inferred
from the counters):

| Transfers per chunk | What |
| --- | --- |
| ~1.0 | carrying the chunk |
| ~0.75 | the ack fetch, which carries nothing toward the node |
| ~0.8 | IRQ-pass visits to slots that bring no record |
| ~0.25 | misses while a node erases flash, with their retries |

## A two-flight window toward the node, 2026-11-07

09082e6 lets the P4 send a node's next flight in the transfer that fetches the previous one's
ack: two flights out per slot toward the node, numbered in the header's flags, taken by the
node strictly in order, and sent again at once from the oldest when a good transfer comes back
without its ack. The uplink keeps its stop-and-wait. Each update session now sends a whole
sector, four chunks, before waiting, so the P4 always has the next chunk queued. To make room,
the P4's queue per slot grew from 4,096 to 4,160 bytes and its bulk queue toward the host
shrank from 256 to 240 KB; its main stack is unchanged at 32 KB.

The nodes moved to the new firmware by `wartui ota` through the old P4, with no wired flash: a
node that has taken no flight since it booted takes the first one whatever its number, and its
confirm was that flight. The P4 was flashed after.

`wartui ota --force --yes`, all ten carrier-10 nodes at once, 4 MHz:

| | fc36e65 | 09082e6 |
| --- | --- | --- |
| Confirmed | 10 of 10 | 10 of 10, twice |
| Rate per node while sending | 22–23 KB/s | 29–30 KB/s |
| Chunk sends and statuses (debug logging) | 8,840 and 2,220 | the same, none undelivered |
| Stuck-IRQ latches per bus | 2–4 | 1 |
| Whole run | 48–50 s | 40 s (42 s with debug logging) |

Over the 40 s run, N1's transfers (from its `xfer` counters, N3, N6 and N8 agreeing):

| | fc36e65 | 09082e6 |
| --- | --- | --- |
| Ack fetches | 659 | 152 |
| Transfers per chunk, all kinds | about 2.8 | about 2.2 |

The visits of IRQ passes are now the largest share, about 1,320 transfers for N1, some of them
carrying chunks.

### After the review fixes (8edcb87)

A code review's fixes changed the rule in one way that matters on the bus: a node that has taken
nothing since it booted now takes only a `SYNC` flight, and the P4 sends `SYNC` whenever a
node's header shows it has taken nothing. Before, a node that rebooted in under 1 s kept its slot
present, could take a later flight ahead of an earlier one, and the P4 would answer both
`Delivered`. In a fleet update, 3 of 10 reboots never went absent. The fixes also pace a second
go-back for the same flight by 25 ms, commit a flight at the node only once it is queued, and
restart the timer of a flight that becomes the oldest.

The P4 went first this time, then the nodes moved by `wartui ota` in 43 s, all ten confirmed:
the old nodes take a `SYNC` flight, so the new P4 drives them, while a new node behind the old P4
could miss its confirm after a quick reboot.

| | 09082e6 | 8edcb87 |
| --- | --- | --- |
| Confirmed | 10 of 10, twice | 10 of 10, twice |
| Rate per node while sending | 29–30 KB/s | 29–31 KB/s |
| Chunk sends and statuses (debug logging) | 8,840 and 2,220 | 8,872 and 2,253 |
| Whole run | 40 s (42 s with debug logging) | 39 s (41 s with debug logging) |

The 32 extra chunk sends are resends, about 0.4%, and none was `Undelivered`.

## Fewer empty IRQ-pass visits, tried and dropped, 2026-11-07

One node updated alone runs at 38 KB/s, against 29–31 KB/s per node with all ten at once, so a
fleet update is partly held back by the bus. Most of what the bus spends that carries nothing is
IRQ-pass visits. A node that has just flushed a sector prepares its status behind a transfer it
armed before the status existed. So the first clock finds that node empty, and the pass walks the
other slots, often twice, before the status comes. With one node updating, each of its four idle
neighbours on the bus was clocked about 600 times in 30 s.

Two P4-only changes were tried, neither committed:

- **Replies first.** A slot whose last flight was just acknowledged, with nothing more queued,
  goes first in IRQ passes for up to 250 ms, since its node is likely about to answer. The first
  empty visit ends the pass, so the next pass starts with that slot again.
- **Round 2, work and replies first.** On top of that, while IRQ is served, each round also
  clocks every slot with work due before the pass. That work includes the ack owed for a record,
  so this one also changed how sighting batches are served.

`wartui ota --force --yes`, all ten carrier-10 nodes, 4 MHz, every node confirmed. The third
run of each ran with debug logging, and the per-sector rows are means from those runs:

| | 8edcb87 | Replies first | Round 2 |
| --- | --- | --- | --- |
| Ten nodes | 39, 40, 41 s | 46, 41, 45 s | 44, 47, 47 s |
| Last chunk delivered to the status at the host | 85.5 ms | 72.8 ms | 88.6 ms |
| First chunk sent to the 4th delivered | 55.7 ms | 69.5 ms | 62.1 ms |
| One node alone | 38 KB/s | 44 KB/s | not run |

Replies first found statuses 13 ms sooner and cut the idle neighbours' visits to about 250, but
while IRQ is asserted, a slot with chunks to send is clocked only by passes, and the shorter
passes reached it less often. Round 2 served those slots in every round, which made both halves
of the sector no better than before. Neither helped the ten-node update, which is the case that
matters, and round 2 put sighting service at risk for nothing, so both were dropped.

Most of a sector's time in a fleet update is the 70–85 ms from its last chunk to its status, and
much of that is the node's own flush. Keeping chunks flowing across a sector's end, so the
flush overlaps the next sector's sending, is the lever left.

## Erasing 64 KB at a time, 2026-11-07

A node takes nothing over its bus while its flash erases or writes: esp-storage runs each
operation in a critical section, so the node's CS-edge handler can't re-arm, and the P4 sees
misses until it ends. Each 4 KB sector's erase and write left a node unreachable for about
43 ms (inferred from the P4's counters in a ten-node update), so a fleet update spent about
7–12 s of each node's time deaf. So keeping chunks flowing across a sector's end, the lever
left above, comes down to how long each flush takes, not to the host's flow.

7c7bd57 has the node erase its update slot a 64 KB block at a time, one ROM call when the image
reaches the block, and only write at the other sectors. The C6 and the P4 keep erasing sector
by sector: they serve links while they update, and their update speed doesn't matter. The P4's
`xfer` line gained `deaf=`, the longest span in each report that a present slot went without a
transfer with the magic. ba59d91, the review's fixes, made the erase unit a property of the
flash target, with no change on the bus.

`wartui ota --force --yes`, all ten carrier-10 nodes, 4 MHz, every node confirmed:

| | 8edcb87 | 7c7bd57 | ba59d91 |
| --- | --- | --- | --- |
| Ten nodes | 39, 40, 41 s | 40, 32 s | 37, 31 s |
| Rate per node while sending | 29–31 KB/s | 34–43 KB/s | 40–43 KB/s |
| One node alone | 37 s, 38 KB/s | 21 s, 67 KB/s | 22 s, 65 KB/s |
| Time each node spent deaf per update | about 7–12 s | about 2 s | — |
| Longest deaf span while sending | under 110 ms | 243 ms | 239 ms |

The first run of each build was the one that moved the nodes onto it. In 7c7bd57's first run,
two nodes sent at 34 KB/s; a spike of the same node change ran in 30 and 33 s. A block erase
costs about 150–170 ms (inferred from the deaf totals), and the longest seen is a quarter of
the P4's 1 s absence span. No node went absent while chunks were sent; every `gone` line was a
node rebooting into its new image.

What remains of a sector's time in a fleet update is mostly the bus sending its chunks.

## The lid fan's duty and speed, 2026-11-07

The P4 now drives the carrier's lid fan, a Noctua NF-A6x15 5V PWM, and reads its tach
(`firmware/p4-bridge/src/fan.rs`). On the carrier-10 under D0:1C, set with `wartui fan` and read
with `wartui status` after 10 s at each duty, three readings 1.5 s apart (each a 1 s count, so
±30 rpm):

| Duty | rpm |
| --- | --- |
| 100 % | 3690–3720 (3900 straight after the flash) |
| 75 % | 2850 |
| 50 % | 1920 |
| 40 % | 1530–1560 |
| 25 % | 870 |
| 20 % | 600–660 |
| 15 % | 390–420, starting from rest too |
| 10 % | 240 stepping down from 15 %; 0 from rest |
| 5 %, 0 % | 0: the fan stops |

So the fan does not turn at a minimum speed at 0 %: it stops, and below 15 % it may not start. The
`fan stopped` fault is raised only at 20 % and above (`FanDuty::TURNS_FROM`). Above 20 % speed
rises close to linearly with duty, about 40 rpm a percent. 100 % ran a little over the fan's
3500 rpm rating, which is ±10 %. Readings taken 2–3 s after a change were still settling: the fan
takes several seconds to slow from 100 % to 40 %.

The duty held across a reboot (`wartui fan 40`, `wartui reset`, 40 % at 6 s of uptime), read back
from the first sector of the P4's `nvs` partition. No step reset the P4.

## Clock sweep on the carrier-10, 2026-11-08

The clock was raised until errors appeared, to see how much margin 4 MHz leaves. The board is a Rev
C carrier-10 in its case, with all ten nodes on 0.4.0. **Soaks counted no errors at the P4 up to
26.67 MHz, and did at 40 MHz.** One update below that counted a CRC error, at 8 MHz. `BUS_HZ` stays
at 4 MHz.

Each step was a P4 image from 27d32c4 with `BUS_HZ` changed and a log line added for each
transfer's duration. Neither change was committed. The P4 divides 160 MHz, and every clock tried
divides it evenly, so each has a 50% duty cycle. Each soak was a `wartui run` with all ten nodes
sniffing and real traffic. An update followed each clean soak: `wartui ota --force --yes` to the
same image, all ten nodes.

| Clock | Shortest transfer | Soak | Transfers | Bus errors | Assignments delivered in | Rate per node | Ten-node update |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 4 MHz | not logged | 5 min | 16,809 | none | 6–22 ms | 40–42 KB/s | 35 s |
| 5 MHz | 1,693 µs | 5 min | 17,860 | none | 6–21 ms | 45–48 KB/s | 34 s |
| 6.67 MHz | 1,276 µs | 5 min | 17,203 | none | 5–17 ms | 53–56 KB/s | 29 s |
| 8 MHz | 1,067 µs | 5 min | 16,898 | none | 4–13 ms | 56–61 KB/s | 27 s |
| 10 MHz | 858 µs | 5 min | 16,766 | none | 4–9 ms | 45–60 KB/s | 32 s |
| 20 MHz | 440 µs | 5 min | 16,539 | none | 3–7 ms | 67–73 KB/s | 27 s |
| 26.67 MHz | 335 µs | 5 + 10 min | 51,037 | none | 3–8 ms | 72–76 KB/s | 24 s |
| 40 MHz, first soak | 232 µs | 5 min | 16,654 | N2 | 3–7 ms | not run | not run |
| 40 MHz, second soak | 232 µs | 5 min | 17,046 | N1, N2 | 3–6 ms | not run | not run |

- **The clocks are what they claim.** Each shortest transfer is its 8,352 bits at that clock, plus
  about 23 µs. The 4 MHz row is the control after the sweep, on `main`'s image.
- **Bus errors** means `magic`, `crc`, `ver`, `len`, `gap` or `retry` rising through a soak, or a
  `gone` line. It leaves updates out, since a node erasing flash misses transfers by design. Every
  soak delivered all 19 assignments and every heartbeat, 12 per node per minute.
- **The soaks carry data up.** At 26.67 MHz each slot ran about 5,100 clean transfers, 4.3 × 10⁷
  bits, which bounds its node-to-P4 bit error rate at ~7 × 10⁻⁸ (95%, inferred from the count). The
  bound is per slot because errors at 40 MHz are specific to a slot. Most of those bits are an idle
  transfer's zeros, so this is not a worst-case pattern.
- **Updates carry data down, and only their outcome speaks for it.** All ten nodes confirmed in
  every update, and each checks the image's digest. A corrupted flight is resent, and the P4
  cannot count that: a node reports its own CRC errors only on its USB console, which wasn't
  attached, and a flight sent in an IRQ pass counts under `pass`, not `carry`. So a rare P4-to-node
  error would not show here. The rising rate says only that errors didn't cost much time.
- **An update's `gone` lines were nodes rebooting.** All fell in the last seconds of a run.
- **The 8 MHz update counted one CRC error, on N2,** in the report as the nodes rebooted. N2 and N9
  each counted one in the supply loss on 3e144f3, 2026-11-07, so a reboot may explain it (not
  tested). N2 is also the slot that fails most at 40 MHz. No other update, and no soak below 40 MHz,
  counted one.
- **Two nodes on bus B set the 10 MHz update's time,** EF:54 and F7:30 at 45 KB/s, against 57–60 for
  the others. Neither stood out at another clock, and the cause was not looked into.

### What failed at 40 MHz

Only bus A failed, and mostly N2. Over the 11 minutes that image ran, soaks and the time between
them:

| Slot | Good transfers | `magic` | `crc` | `gap` | `retry` |
| --- | --- | --- | --- | --- | --- |
| N1 | 3,943 | 2 | 0 | 2 | 2 |
| N2 | 3,965 | 8 | 4 | 12 | 8 |
| The other eight | 3,942–4,126 each | 0 | 0 | 0 | 0 |

- **N2 failed in both soaks, and bus B had no error in either.** So 40 MHz is not a hard limit for
  every slot, and it is inside the 60 MHz ESP-IDF rates the C5's SPI slave to.
- **Each miss was a single transfer.** `gap` equals `magic` plus `crc` on both slots, and no slot
  went `gone`. The P4 retries only a bad magic, and `magic` equals `retry`, so every such retry was
  answered. A CRC error gets no retry, and the node resends its records when no ack comes. N2's
  errors, about 0.3% of its transfers, cost it no heartbeat in either soak.
- **`stuck` rose too, on bus A only,** three times across the soaks. All three fell in the same 5 s
  report as an N2 miss, so a miss may be what empties the pass (inferred from the timing).
- **Whether this follows the slot or the node is open.** The nodes were not swapped, since they sit
  in the case with their antennas. 00:40 was in N2 throughout.

### Limits of this sweep

- 80 MHz was not tried: the sweep stopped at the first failure, and it is past the C5's rating.
  ESP-IDF v6.1's SPI slave page gives no MISO timing for the C5.
- No scope was used. Everything above is what the two ends counted, so it shows where errors
  appear and not what the signals look like.
- It covers this one board, in its case. The 14-node carrier is untested, and a clean 26.67 MHz
  here says nothing about it.

## 8 MHz on the full carrier-10, 2026-11-08

`BUS_HZ` went from 4 MHz to 8 MHz (10c5f19), with nothing else changed. That leaves over 3× margin to
26.67 MHz, the fastest clock that soaked clean in
[the sweep](#clock-sweep-on-the-carrier-10-2026-11-08). The board and the ten nodes are the sweep's,
on 0.4.0. The soak was a `wartui run` with all ten sniffing and real traffic. The updates were
`wartui ota --force --yes` to the same image: all ten twice, then 0D:20 alone.

| | Sweep's 8 MHz row | 10c5f19 |
| --- | --- | --- |
| Soak | 5 min | 10 min |
| Transfers | 16,898 | 32,413 |
| Bus errors in the soak, as the sweep defines them | none | none |
| Stuck IRQ in the soak | not counted as an error | none |
| Assignments delivered in | 4–13 ms | 4–14 ms, 19 of 19 |
| Heartbeats | 12 per node per minute | 120 per node, 12 per minute |
| Bridge frames dropped | not recorded | 0 of 1,529 |
| Rate per node, ten nodes | 56–61 KB/s | 56–60, then 56–61 KB/s |
| Ten-node update | 27 s | 27, then 25 s |
| Confirmed | 10 of 10 | 10 of 10, twice |
| CRC errors in updates | 1, on N2 | none |

The whole run includes the 6 s listen and an incremental image build. The second ten-node run found
the image already in the P4's cache.

- **Latency fell.** IRQ-to-transfer latency had a mean per 5 s window of 7–9 ms and a maximum since
  boot of 12 ms. cb73e56 at 4 MHz had 16–18 ms, and 22 ms on bus A and 76 ms on bus B at boot. That
  is an older build, so not all of the fall is the clock's.
- **One node alone took 19 s at 86 KB/s,** against 22 s at 65 KB/s at 4 MHz (ba59d91).
- **The sweep's CRC error did not recur.** `crc` stayed 0 on every slot through these three updates
  and the two below, read again in a 45 s capture after each round. N2's errors at 40 MHz are
  unchanged as a question, and the sweep's single one at 8 MHz stays unexplained.
- **A node's flush left it deaf about as long as at 4 MHz,** at most 247 ms while sending. No node
  went `gone` while chunks were sent.
- **This counts nothing toward a node,** as in the sweep: no node's USB console was attached. All
  that speaks for the downlink is that every update confirmed, each checking the image's digest.

### The stuck latch and seq gaps against a 4 MHz control

Two more rounds the same day, on the same board: flash the P4, two ten-node updates, then a 45 s
capture to read the counters after the last reboots. 4 MHz (75dbfcf) ran first, then 8 MHz (10c5f19).
The `stuck` and rate rows add the two ten-node updates above to the 8 MHz column. The `magic` and
`gap` rows are these two rounds alone, summed over the ten slots.

| | 4 MHz | 8 MHz |
| --- | --- | --- |
| Confirmed | 10 of 10, twice | 10 of 10, four times |
| Whole run | 38, 30 s | 27, 25, 28, 25 s |
| Rate per node | 33–42, 40–42 KB/s | 56–60, 56–61, 56–61, 56–61 KB/s |
| `stuck` rise per update, bus A | 3, 0 | 3, 3, 3, 3 |
| `stuck` rise per update, bus B | 6, 0 | 5, 4, 2, 3 |
| `crc` | 0 | 0 |
| `magic` over two updates | 2,124 | 5,260 |
| `gap` over two updates | 122 | 599 |

- **The latch fires at both clocks.** It rose only in the reports that span reboots, and stayed 0
  through the soak and through sending. One 4 MHz update rose more than any at 8 MHz, and the other
  not at all, so two runs show no clock effect and cannot rule out a small one. `EMPTY_PASSES` counts
  passes, so its window shrinks with the clock, from about 23 ms to about 13 ms with five nodes
  (inferred from the transfer times). A latch costs at most `STUCK_RETRY` of polling, so it stays as
  it is.
- **`gap` during an update peaks near 8 MHz.** The sweep's logs, one ten-node update each, summed
  over the ten slots at the last report before the command exited:

  | Clock | 4 MHz | 5 MHz | 6.67 MHz | 8 MHz | 10 MHz | 20 MHz | 26.67 MHz |
  | --- | --- | --- | --- | --- | --- | --- | --- |
  | `magic` | 824 | 1,153 | 1,431 | 2,371 | 4,397 | 5,052 | 5,262 |
  | `gap` | 45 | 111 | 179 | 304 | 156 | 30 | 15 |

  The control's 4 MHz and 8 MHz rounds agree with that difference. A gap means a node's seq jumped
  between good transfers, so transfers were lost or the node restarted. These build up while chunks
  are sent, not at the reboots: N1's count at 8 MHz read 4, 14, 23 and 26 over successive 5 s
  reports. No soak below 40 MHz counted one, and every update confirmed, so no cost shows. The cause
  was not looked into.

## Still open

This list replaces the dated ones above.

- A ten-node capture with the real antennas, for coverage.
- Two fixes that need conditions this bench can't easily create: retrying each expander
  transaction at boot, which needs an I2C glitch, and exempting only the node in a cycle now
  from the supply-loss count, which needs the supply to drop within about 1 s of a queued cycle.
- Whether the 40 MHz errors on N1 and N2 follow the slot or the node, which needs nodes swapped.
- How many P4-to-node errors each clock causes, which needs a node's USB console attached.
- Why seq gaps during an update peak near 8 MHz.
- The 14-node carrier.
