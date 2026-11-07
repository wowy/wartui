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

### Still open

- A ten-node capture with antennas, for coverage.
- Two fixes that this bench can't provoke: retrying each expander transaction at boot, which
  needs an I2C glitch, and exempting only the node in a cycle now from the supply-loss count,
  which needs the supply to drop within about 1 s of a queued cycle.
- 4 MHz.
