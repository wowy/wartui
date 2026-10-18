# The P4 split bridge: measured against the single-chip bridge

These measurements back the P4 split bridge. The design is in
[`docs/superpowers/specs/2026-10-17-p4-split-bridge-design.md`](superpowers/specs/2026-10-17-p4-split-bridge-design.md),
and the SPI link spike is in [`p4-spi-link-findings.md`](p4-spi-link-findings.md).

Every run uses the same load:
- **Nodes:** two, both running the burst-test node build from local branch
  `bench/burst-load-1.2` (f67ec33). They are `F5:50` (ESP32-C5) and `9A:24` (XIAO ESP32-C6
  with an external antenna).
- **Host:** `wartui run` for 10 minutes with `--no-gps` and a fixed test position, then
  `wartui analyze`.
- **Bench:** the P4 board stays unplugged during the single-chip runs, because its spike C6
  announced itself as core.

## Baseline: single-chip bridge, blocking send (2026-10-17)

The bridge is `75:40` (XIAO ESP32-C6 with an external antenna), running `firmware/bridge` at
f71b902, which is `main` plus documents only.

| | |
|---|---|
| span | 10 min (03:13–03:23 UTC) |
| frames the bridge received | 11,373 |
| frames its USB outbox dropped | **1,425 (12.5%)** |
| frames the host read | 9,962 |
| batches lost, node to host | 1,425: `F5:50` 647, `9A:24` 778 |
| heartbeats missed | 0 of 272 |
| host behind ≥100 ms | once, worst 14.4 s, at attach while the backlog drained |
| assignments | 2, both acked; latency 1.0 ms and 3.9 ms |

**Every lost batch is an outbox drop.** The 1,425 batches lost between node and host equal the
1,425 frames the outbox discarded. None were lost on the air, and none in the radio's
receive queue. Under this burst load, the single-chip bridge's USB path is what limits the
fleet. That is the headroom the P4 bridge is meant to add.
