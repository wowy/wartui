# Attaching to a bridge that ran without a host

What a host reads when it attaches to a bridge that has been running with nobody reading, before
and after #218. Bench of 2026-10-19: nodes `F5:50` and `9A:24` on production firmware, the host
attached with `wartui run` for 40–60 s, and each capture read back with `wartui analyze`.

## Before: the backlog replayed

On `main` at `8084b93`, the T-Dongle-C5 (`44:C0`) was left 3 minutes with no host, then attached.
The host fell up to 56 s behind the bridge while the outbox drained, counted 27 admin windows
missed that were never live, and read 1 garbled frame. The P4 showed the same thing at a larger
scale, because its bulk queue is 256 KB rather than a few kilobytes
([`p4-bridge-findings.md`](p4-bridge-findings.md): 243 frames, 78 s at worst).

## After: no host reads a backlog

Three pieces, each needed:

- **The bridge queues nothing before a host and clears its queue when one connects.** The
  single-chip bridge queues nothing until a host first speaks and clears the queue on `Identify`.
  It cannot see a host leave, since USB Serial/JTAG gives no DTR level, so after a quit up to a
  ring's worth queues again until the next `Identify` clears it. The P4 uses DTR and clears on its
  rising edge.
- **The host drops what decodes before the connection's first `Ready`.** Bytes already past the
  bridge's queue when the last host quit, in the endpoint's FIFO or the tty's buffer, still
  arrive. With only the bridge change, a restart 3 s after a quit read two heartbeats 1.3 s and
  5 s old, fell 4.2 s behind and counted 3 admin windows missed. After the clear, the bridge
  queues `Ready` first, so anything decoding ahead of it predates the connection.
- **The engine starts a connection caught up.** Its old assumption that the host started behind
  the air was there for the backlog. After a bridge reset it cost 3 admin windows, because an
  11 µs difference between bridge-elapsed and host-elapsed time held the assumed 100 ms lag at
  the threshold for 5 s.

With all three in place:

| Bridge | Case | Behind ≥100 ms | Admin windows missed | Stale frames skipped by the host |
| --- | --- | --- | --- | --- |
| T-Dongle-C5 `44:C0` | attach after 3 min idle | never | 0 | 5 |
| | quit, restart after 3 s | never | 0 | 3 |
| | `wartui reset`, then attach | never | 0 | 0 |
| C6 `75:40` | attach after 3 min idle | never | 0 | 0 |
| | quit, restart after 3 s | never | 0 | 3 |
| | `wartui reset`, then attach | never | 0 | 0 |
| P4 `D8:1C` | attach after about 7 min idle | never | 0 | 0 |
| | quit, reattach after 30 s | never | 0 | 0 |

The C6's first attach read 1 garbled frame 2 ms after the port opened: the fragment the bridge
cut off when it cleared, ended by its `0x00`. On the P4, `Ready` answered within 2 ms of the
port opening on both attaches, so the bridge saw DTR rise before it read `Identify`. Each P4 run
missed 1 heartbeat from `9A:24`, which that board's C6 hears weakest; that is reception, not the
attach. No bridge had a GPS fix on this bench, so the GPS half of the rule was not exercised.
