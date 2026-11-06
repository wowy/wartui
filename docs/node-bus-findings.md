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
