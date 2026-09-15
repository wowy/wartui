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

### Raspberry Pi CM5, Intel Optane 16 GB NVMe

`drive`, 300 s, three runs, kernel 6.18, ext4 mounted `noatime`. The medians are below, and every run agreed
with them to within 0.5%. Idle node-windows 0 and rows dropped 0 in all three.

| figure | median |
| --- | --- |
| rows/s | 11,531 |
| commits/s, rows/commit | 22.5, 511 |
| commit p50 / p95 / p99 ms | 1.33 / 62.4 / 63.7 |
| batch p50 ms | 4.90 |
| write syscalls | 5.85 M (865 per commit) |
| MiB handed to the kernel | 14,928 (2.2 MiB per commit) |
| device writes | 1.74 M (5,800/s) |
| device MiB | 15,021 |
| KiB per device write | 8.84 |
| database at close | 236 MiB |
| peak RSS | 9.6 MiB |
| export of 1.74 M observations | 14.8 s |

Per minute (run 1):

| minute | device MiB | KiB/write | commit p99 ms |
| --- | --- | --- | --- |
| 1 | 2,143 | 11.3 | 53.1 |
| 2 | 3,045 | 9.0 | 61.4 |
| 3 | 3,273 | 8.5 | 63.4 |
| 4 | 3,275 | 8.4 | 63.6 |
| 5 | 3,282 | 8.3 | 64.7 |

What it says:

- **The store writes about 64 bytes to the device for every byte the database
  grows.** 15 GiB went out to grow the file by 236 MiB. The process itself handed the
  kernel 2.2 MiB per commit, for 511 rows that grow the file by about 36 KiB. Once
  settled that is roughly 55 MiB/s, or about 196 GiB per hour of driving.
- **The writes are small and scattered, and they get smaller as the file grows.** At
  8.8 KiB per request, the load is essentially random 4 KiB pages, and per-write size
  falls from 11.3 to 8.3 KiB over five minutes. That is the signature of a
  random-keyed index (`obs_bssid`): each commit dirties leaf pages spread over the
  whole index, and there are more of them to hit as it grows.
- **Commits are bimodal.** p50 is 1.3 ms and p95 is 62 ms, with almost nothing in
  between, which fits some commits also running SQLite's automatic checkpoint. That
  copies the WAL's pages back into the database file, so every dirty page is written
  twice.
- **A microSD card is unlikely to sustain this.** The Optane took 5,800 random writes
  a second and still averaged 3.1 ms per request once queueing is counted. An A2-rated
  card is only specified for 2,000 random-write IOPS at 4 KiB, and 196 GiB an hour is
  an endurance problem for any card. Not yet measured on one.
- **Memory is not the constraint.** 9.6 MiB peak with SQLite's default ~2 MiB cache.
- **`device_flushes` is 0, and that is likely the drive, not the store.** A device
  that reports no volatile write cache is never sent flush requests.
- **`storage_write_mib` counts whole kernel pages, and this kernel's are 16 KiB.**
  `rpi-2712` kernels use 16 KiB pages, so a 4 KiB SQLite page write that dirties a
  page-cache page is charged at 16 KiB. That is why it reads 33.5 GiB against 14.9 GiB
  actually written; on the x86 laptop below, with 4 KiB pages, the two nearly agree.
  Read `write_mib` and the device columns instead.

### Raspberry Pi CM5, Amazon Basics 64 GB microSD

`drive`, 300 s, three runs, the same board and kernel as the Optane runs above, and
ext4 mounted `noatime` there too.
Idle node-windows was 0 in every run, so the fleet produced the same load. The
card could not keep up with it.

| figure | median |
| --- | --- |
| rows/s | 3,633 |
| rows dropped | 2.36 M of 3.46 M (68%) |
| commits/s, rows/commit | 7.1, 512 |
| commit p50 / p95 / p99 / max ms | 1.31 / 690 / 721 / 759 |
| write syscalls | 1.56 M |
| MiB handed to the kernel | 3,854 (1.8 MiB per commit) |
| device writes | 369 k |
| device MiB | 3,934 |
| KiB per device write | 10.8 |
| device write ms per request | 13.3 |
| database at close | 76 MiB |
| export of 560 k observations | 4.2 s |

Per minute, as the median of the three runs:

| minute | rows/s | dropped | commit p99 ms | device writes | device MiB | KiB/write |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 6,122 | 320,521 | 497 | 61,710 | 936 | 15.1 |
| 2 | 3,420 | 484,992 | 646 | 74,004 | 776 | 10.7 |
| 3 | 3,036 | 505,440 | 699 | 74,951 | 756 | 10.0 |
| 4 | 2,900 | 517,344 | 716 | 79,596 | 741 | 9.5 |
| 5 | 2,806 | 524,538 | 738 | 80,961 | 736 | 9.4 |

What it says:

- **The store drops most of what it is given from the first minute on.** It dropped
  320 k rows in minute 1 and 525 k in minute 5.
- **The card is at its random-write ceiling, and the work per row keeps rising.** Its
  request count creeps up (61 k to 81 k a minute, about 1,350/s by minute 5) while
  bytes per request fall (15 to 9.4 KiB). Throughput was still falling when the run
  ended, so five minutes did not find the floor.
- **Commits are fast or about 0.7 s, with little in between.** At 7.1 commits/s the
  writer averages 141 ms a commit, which works out to roughly one commit in five being
  a slow one. That fits SQLite's automatic checkpoint (every 1000 pages) running
  inside the commit and waiting on the card. It is inferred from the percentiles, not
  counted. Each 0.7 s stall lets about 8,000 rows arrive against a 4,096-record queue.
- **Write amplification is the same shape as on the Optane.** 3.9 GiB went to the card
  to grow the file by 76 MiB, about 52×.

### Lenovo ThinkPad X1 Carbon Gen 13, Crucial T500 NVMe

`drive`, 300 s, three runs, Fedora 44, kernel 7.2, x86_64, **on battery**. btrfs
mounted `relatime,compress=zstd:1,discard=async`, no encryption. The device counters
are empty because btrfs gives files an anonymous device number with no
`/sys/dev/block` entry, so `bench` could not name the device. It now falls back to the
mount's source, and reports the filesystem, its options and whether the machine was on
battery.

| figure | median |
| --- | --- |
| rows/s | 11,514 |
| rows dropped | 3,474, then 3,534 and 0 (about 0.1%) |
| commit p50 / p95 / p99 ms | 3.30 / 57.3 / 64.3 |
| commit max ms | 602, 563 and 87 |
| write syscalls | 5.84 M |
| MiB handed to the kernel | 14,891 |
| database at close | 236 MiB |
| peak RSS | 11.2 MiB |
| export of 1.73 M observations | 7.5 s |

What it says:

- **The process writes the same thing on every machine.** 14.9 GiB of writes to grow
  a 236 MiB file, which is within 0.3% of the CM5 on Optane. The amplification belongs
  to the store and SQLite, not to the device or the filesystem.
- **A single stall of about 0.6 s drops rows even on a fast SSD.** Both runs that
  dropped did so in the first two minutes, and both had one commit of 560–600 ms. At
  11,500 rows/s the 4,096-record queue holds about 355 ms of headroom.
- **A median commit costs 3.3 ms against 1.3 ms on the Pi.** That compares two
  machines, not two drives. btrfs with zstd compression does more work per write than
  ext4, and on battery the CPU is likely held to a lower performance level. The 0.6 s
  stalls could come from either. A run plugged in, with the device counters, is the
  one to compare.

## Experiments

### Deferring the `obs_bssid` index

**Hypothesis.** Most of the write amplification is `obs_bssid`. Its key is a random
address, so every commit dirties leaf pages spread across the whole index, and each
page is written twice: to the WAL, then into the file at checkpoint. The other writes
mostly append. Leaving the index unbuilt during capture should cut the pages per
commit from hundreds to tens, and the rows the card can take should rise with it.

`--defer-bssid-index` creates the schema without the index and builds it once, after
the last batch. The build is timed as `index_build_ms` and left out of the rates. Its
I/O is in the totals, since a capture would pay it, but not in the timeline. Run it
beside the baseline on the same device:

```sh
for i in 1 2 3; do
  target/release/wartui bench --db /mnt/card/bench.db --fresh --duration 300 --defer-bssid-index --json
done
```

To be measured. If it pays off, making it the store's behaviour still needs two
things decided: how an export taken during a capture finds its networks without the
index, and whether an index built in one go at close is affordable on a card after an
hour's drive.
