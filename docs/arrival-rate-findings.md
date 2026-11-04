# Arrival rate — peak sightings per second and per minute

Measured on 2026-11-03 with `wartui analyze` over the three largest captures this build opens: a
HackberryPi CM5 host behind the P4 split bridge, on three drives of six to nine nodes. The
question was how fast sightings cross the link at its busiest, to size future features and
bandwidth. Peaks are sliding half-open windows over the host's arrival time, every stored row
counted (`crates/wartui-core/src/analyze.rs` `//!`).

## The drives

| Drive | Span | Nodes | Sightings |
| --- | --- | --- | --- |
| 2026-10-31 | 2h 15m | 6 | 76,273 Wi-Fi |
| 2026-10-24 | 2h 9m | 9, one on BLE | 55,111 Wi-Fi, 16,706 BLE |
| 2026-11-01 | 1h 35m | 6 | 52,398 Wi-Fi |

## Findings

| Drive | Fleet /s | Fleet /min | Wi-Fi /s | Wi-Fi /min | BLE /s | BLE /min |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-31 | 105 | 2,058 | 105 | 2,058 | — | — |
| 2026-10-24 | 105 | 1,931 | 102 | 1,574 | 53 | 452 |
| 2026-11-01 | 91 | 2,866 | 91 | 2,866 | — | — |

Per node:

| Drive | Busiest node /s | Busiest node /min | Node range /s | Node range /min |
| --- | --- | --- | --- | --- |
| 2026-10-31 | 43 (`10:D0`) | 478 (`10:D0`) | 25–43 | 255–478 |
| 2026-10-24 | 53 (`75:40`, BLE) | 452 (`75:40`) | 12–53 | 99–452 |
| 2026-11-01 | 35 (`10:D0`) | 567 (`0A:CC`) | 23–35 | 396–567 |

- The fleet's one-second peak sits at 91–105 sightings on every drive, with six nodes or nine.
  Nodes burst at different instants: the per-node one-second peaks sum to 165–255, well above the
  fleet's.
- The fleet's one-minute peak is 1,931–2,866 sightings, under 48 per second averaged, so the
  sustained load is about half the burst.
- No Wi-Fi node exceeds 43/s or 567/min. The one BLE node peaks higher per second (53) than any
  Wi-Fi node, on a drive where it also heard 71 Wi-Fi sightings.
- The link had headroom throughout: the bridge dropped nothing on any drive, the `host-bulk`
  queue peaked at 666–731 of 262,144 bytes, and no batch was lost between node and host.
- No drive showed a wall-clock reversal, so no window restarted.

## What it means

A consumer of the live sighting stream must absorb bursts of about 100 sightings per second
from a fleet this size and keep up with about 50 per second sustained. Per node, 55 per second
and 600 per minute cover every drive here. Bytes follow from these counts and the SSID length
chosen for the plan (average or the 32-byte worst case), which is why `analyze` reports counts.

Three drives on one route set are a floor, not a ceiling. A denser area, more nodes or a BLE
node on every board would raise the fleet figure, and the per-node peaks sum to more than twice
the fleet's, so a plan that must survive nodes bursting together should size from that sum.
