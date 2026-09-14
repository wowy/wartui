# Store I/O findings

What the store costs the card it writes to, and what changing it bought. The question
behind it: a wardriving box boots from a microSD card, and a card is slow at exactly
what the store does most — many small writes scattered across a file — while the
process leaves most of the machine's RAM unused.

## Method

`wartui bench` (`crates/wartui/src/bench/`) runs the simulator through the engine
into a fresh database for a fixed time, with nothing drawn, and reports:

- **from the writer:** rows written and dropped, commits, and batch and commit times
  at p50/p95/p99/max
- **from `/proc/self/io`:** write syscalls, bytes handed to the kernel, and bytes
  sent to storage
- **from the block device's `stat`:** completed write requests, bytes, time spent
  writing, and flushes. Taken after a `sync`, so delayed writeback counts against
  the run that caused it. `device_kib_per_write` is the headline figure: bigger means
  fewer, longer writes.

The device counters cover the whole device, so every other writer on the card is in
them. Run on an otherwise idle machine, take three runs per setting, and compare
medians.

The report also carries a `timeline`: the same figures for every `--interval` (60 s
by default), cut at the first 2 s sample past each boundary. Totals hide a card that
slows down part-way through, which is what these tend to do:

- **the card's write cache fills**, after hundreds of MB to a few GB
- **the database outgrows SQLite's page cache**, so index pages start coming back
  off the card
- **the board gets hot** and throttles

A slice's device figures trail the store's by the kernel's writeback delay, up to
about 30 s. Nothing is synced between slices, since that would change the writeback
being measured, so read the device columns as a trend.

Five minutes is enough to compare settings. Once per device, run 30–60 minutes to
find where it settles, and check free space first: `drive` writes roughly 3 GB an
hour.

```sh
cargo build --release -p wartui
for i in 1 2 3; do
  target/release/wartui bench --db /mnt/card/bench.db --fresh --duration 300 --json
done
target/release/wartui bench --db /mnt/card/bench.db --fresh --duration 3600 --json
```

The SQLite and batching settings are flags (`--commit-interval`, `--commit-rows`,
`--queue-depth`, `--cache-mib`, `--wal-autocheckpoint`, `--page-size`), and
everything left unset is the build's default. That lets a baseline and a candidate
run on the same binary.

### What the load is

Every node is busy for the whole measured window, and the report proves it.
`drive` is ten nodes in real time and `burst` is twenty, the fleet maximum. In both,
one node scans Bluetooth.

- **Neighbourhood size.** The simulator's nodes dedup through the same 200-entry ring
  the firmware links, so a node hearing no more networks than that reports them once
  and falls silent. Once it hears more, it re-reports everything on every sweep:
  eviction is oldest-first in a fixed sweep order. So the bench sizes the
  neighbourhood from the node count, giving the node with the fewest channels one and
  a half rings' worth: 4000 networks for ten nodes, 12000 for twenty.
- **Warm-up.** Nodes park until assigned, and each join re-cuts every assignment. The
  clock, the kernel counters and the commit figures start only once every node holds
  an acknowledged assignment and has reported a sighting (about 3 s).
- **Proof.** Every 2 s each node's observation count is checked. `idle_node_windows`
  counts the windows in which a node heard nothing, and anything but zero means the
  run is not what its profile says. The human report prints a warning when it happens.

## Baseline

The store as of this benchmark: WAL, `synchronous=NORMAL`, a commit every 512 rows or
100 ms, and SQLite's defaults for everything else (about 2 MiB of cache, a checkpoint
every 1000 pages, 4 KiB pages).

### Laptop SSD, macOS: a smoke test with no kernel counters

30 s runs. These show the bench works and what the load looks like; they say nothing
about a card.

| profile | idle node-windows | obs/s per node | rows/s | commits/s | rows/commit | commit p99 ms | dropped |
| --- | --- | --- | --- | --- | --- | --- | --- |
| drive | 0 | 497–672 | 11402 | 22.3 | 511 | 19.1 | 0 |
| burst | 0 | 827–1738 | 49391 | 96.5 | 512 | 22.4 | 106615 (7%) |

Already visible:

- **The writer is the ceiling, not the disk.** `burst` drops rows even on a laptop SSD.
  Every batch fills to the 512-row limit, so the writer spends its time running
  statements one row at a time.
- **About two rows per sighting.** `rows_written` is about twice `observations_heard`,
  because every received frame also upserts its node's row. The `node` table never
  holds more than twenty rows, so half the statements the writer runs keep rewriting
  the same handful.

### Raspberry Pi, microSD

To be measured.

### x86 laptop, Linux

To be measured.
