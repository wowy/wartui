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
  at an unarmed slave, and none ended before its DMA reported done.
- **The IRQ latency is about one transfer.** A transfer takes 8.3 ms at 1 MHz. When a heartbeat
  is queued, the node already has an idle transfer armed, so the P4 clocks that one first and
  collects the record on its next pass. The 18 ms maximum is two passes. At 4 MHz, a pass is
  about 2 ms.
- **`replaced` did not grow during the run.** It counts heartbeat copies replaced by a newer one
  before the P4 clocked them. It already stood at 15 and 6 at the first reading, 40 s in, and
  stayed there. The nodes were flashed and running before the P4 was, so those copies most
  likely queued while nothing polled.

## The DMA is done before the edge's handler looks

A later build (db7155d) bins each CS edge by how long the handler waits for the slave's DMA to
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
reboot from either node. So serving the slave from a GPIO interrupt did not disturb the
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
