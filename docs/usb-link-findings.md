# USB link — moving a whole frame per loop pass

Measured on 2026-10-15, on the bench at close range. The question was how much of a long frame's
cost is the bridge's USB link, and how much of it a pass that stays on the link wins back
(`crates/wartui-proto/src/link/burst.rs`, issue #187).

**Boards and firmware**

- Bridge: `00:08`, a XIAO ESP32-C6 with an external antenna.
- Nodes: `C5:B8`, an ESP32-C5, and `8B:90`, an ESP32-C6.
- Host, nodes and the "before" bridge were built from `main` at d5df931. The "after" bridge
  adds `link_burst` with `BURST_GAP_US` 200 and `BURST_BUDGET_US` 1000.

**How it was run**

- **Host→node:** `wartui ota --force` to both nodes, logged at `RUST_LOG=wartui_core=debug`. Each
  1035-byte chunk was timed from the host's send to its `SendResult`, with one chunk in flight at a
  time.
- **Node→host:** each node ran a bench-only build that offered 64 made-up access points every
  dwell, so most batches were full. The patch was never merged. The host logged each `Rx` as it
  decoded, with the bridge's `rx_us`. Each run lasted 5 minutes, with a static test position.
  - Lag is the host's arrival time minus `rx_us`, relative to the smallest such offset in the run.
  - Spacing is the time between the host's arrivals of two frames, for a batch of 900 bytes or
    more that the bridge received before the frame ahead of it reached the host. That is the time
    to drain one such frame through USB.

## Host→node chunks (1035 bytes)

| node | bridge | chunks | median | p10 | p95 |
| ---- | ------ | ------ | ------ | --- | --- |
| 8B:90 (C6) | before | 879 | 4.54 ms | 3.97 | 12.9 |
| 8B:90 (C6) | after  | 870 | 3.54 ms | 3.05 | 9.9  |
| C5:B8 (C5) | before | 832 | 4.03 ms | 3.95 | 6.9  |
| C5:B8 (C5) | after  | 832 | 3.28 ms | 3.03 | 7.2  |
| C5:B8 (C5) | after, 3 ms budget | 832 | 3.22 ms | 2.97 | 8.5 |

## Node→host sighting batches

| bridge | frames | of them ≥ 900 B | lag, median / p95 | spacing of queued 1 KB batches, median |
| ------ | ------ | --------------- | ----------------- | -------------------------------------- |
| before | 8,703 | 4,323 | 3.81 / 5.02 ms | 1.80 ms (~570 KB/s) |
| after  | 8,721 | 4,349 | 3.14 / 4.46 ms | 1.52 ms (~670 KB/s) |

No frame failed to decode in either run.

## Findings

- **A 1035-byte chunk crosses about 0.8–1.0 ms faster**, a 19–22% cut on both chips. Every chunk
  in all five update runs was acked, and every update confirmed.
- **Forwarded batches are about 0.7 ms sooner at the median.** Back-to-back 1 KB batches drain
  about 18% faster.
- **The 1 ms budget does not bind.** Tripling it moved the C5's median by 0.06 ms, which is within
  run-to-run noise. A burst already finishes a chunk well inside 1 ms, so the larger budget would
  buy only a longer stall of the radio's receive queue.
- **What is left of a chunk's ~3.3 ms is not the USB link.** The baseline's 4.0–4.5 ms is slower
  than the 3.6 ms that `docs/espnow-v2-findings.md` measured with other boards. That makes these
  absolute numbers a property of this bench, and the before/after difference the result. The
  bridge's `tx_us` advances in step with the host's clock, chunk to chunk, so these logs cannot
  split the rest among:
  - the radio's send-complete wait;
  - the `SendResult` trip back;
  - the host's turnaround.

## The stall reset, with the burst in place

Run on 2026-10-15 on bridge `00:08` at 0.1.1, with `C5:B8` and `8B:90` heartbeating. The method
was `docs/phase-3-findings.md`'s:

- A bench-only build stood in for the endpoint stopping. Its `UsbSink` refused every byte from
  20.0 s of the bridge's uptime, and it logged a marker 200 ms before.
- A host sent one `Identify`, then a `GetStatus` every 500 ms. Host times below are from opening
  the port.

| scenario | result |
| -------- | ------ |
| wedged, host polling throughout | Endpoint stopped at 20.28 s. The reset came 3.0 s after the first poll it could not answer, and the next `Ready` arrived at 23.89 s, reporting `TxStalled`. |
| wedged, host silent from 20.5 s to 31.0 s | No reset and no bytes while silent. The reset came 3.0 s after the host returned, with `Ready` at 34.40 s, reporting `TxStalled`. |
| main loop blocked 8.2 s, not a wedge | No reset. Status replies resumed 8.3 s later, and one life ran to 44 s. |
| capture quit with heartbeats arriving, then 25 s unread | No reset. The bridge was still on the life it had before the capture. |

- **All four agree with the phase-3 rows.** Each `Ready` lands about 0.4 s after the reset, which
  is the reboot to the main loop.
- **A burst never kept the stall clock from starting.** Against a refusing endpoint the burst
  writes nothing, so its pass reports `moved` false.
