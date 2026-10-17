# Burst bench

Bench-only, never merged. This branch keeps the setup used for issue #202 (bridge USB drain under
burst load), so it can be run again. Findings: `docs/link-encode-findings.md`.

## Nodes

`firmware/node` gains a `burst-test` feature (`src/burst.rs`). Every 10 s, once a node has a
core, it sends a burst of 46, 91 or 132 full (~1 KB) sighting batches of fabricated access points
through the real acked path. The start is offset per node, and the bursts drift against each other.

    wartui flash-fleet --features esp32c5,burst-test --firmware-dir firmware/node
    wartui flash-fleet --features esp32c6,xiao-external-antenna,burst-test --firmware-dir firmware/node

## Comparing two bridge builds

Compare by drain rate, never by loss: loss follows how the nodes' bursts overlap, which drifts
between runs. Alternate the builds run by run, with a checkout of each:

    export BRIDGE=B2:54 WARTUI=target/release/wartui
    tools/burst-bench/ab.sh m1 ../main/firmware/bridge
    tools/burst-bench/ab.sh b1 firmware/bridge
    tools/burst-bench/ab.sh m2 ../main/firmware/bridge
    tools/burst-bench/ab.sh b2 firmware/bridge

Each prints the drain while frames arrive and once the radio is quiet. One undecodable line per
run, the partial frame at the port opening, is normal.

- `stamp.py` prefixes each `sniff` line with the host's arrival time.
- `drain.py` reads a stamped capture for the two drain rates.
- `bursts.py` lists each node's bursts with receive counts and seq gaps, from an unstamped capture.

## Afterwards

Put the production firmware back on the bridge and the nodes.
