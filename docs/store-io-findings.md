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
- **Turnover.** The store is sized for one drive of at most 2 M sightings of 500 k
  networks. Four sightings per network is the ratio of the operator's own drives, and
  500 k is the most WDGWars accepts in a day. So a network's slot takes a new address
  after four hearings (`--sightings-per-address`, 0 to turn it off). At about 5,750
  sightings a second, a 350 s run is one such drive. Runs recorded before this was
  added had a fixed neighbourhood of about 5,300 addresses.

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

**CM5 on the Amazon Basics microSD**, ext4 `noatime`, three runs of `drive` for
300 s. The baseline column is the medians from the card section above.

| figure | baseline | deferred |
| --- | --- | --- |
| rows/s | 3,633 | 11,534 |
| rows dropped | 68% | 0 in every run |
| commit p50 / p95 / p99 / max ms | 1.31 / 690 / 721 / 759 | 0.12 / 3.8 / 125 / 157 |
| batch p50 ms | 4.99 | 2.22 |
| write syscalls | 1.56 M | 458 k |
| MiB handed to the kernel | 3,854 | 1,051 |
| device writes | 369 k | 11.5 k |
| device MiB | 3,934 | 1,019 |
| KiB per device write | 10.8 | 90.4 |
| database at close | 76 MiB | 234 MiB |
| device MiB per MiB of database | 52 | 4.4 |
| index build at close | — | 2.7 s |
| export | 4.2 s (560 k rows) | 14.7 s (1.74 M rows) |
| peak RSS | 10.6 MiB | 12.6 MiB |

Over the timeline the deferred runs held about 11,530 rows/s and 190–195 device MiB
a minute, from the first minute to the fifth. Commit p99 stayed between 119 and
129 ms, and nothing degraded.

**ThinkPad**, on battery for both, btrfs. The baseline column is the medians from
the ThinkPad section above.

| figure | baseline | deferred |
| --- | --- | --- |
| rows/s | 11,514 | 11,520 |
| rows dropped | 3,474 in two of three runs | 0 in every run |
| commit p50 / p95 / p99 / max ms | 3.30 / 57.3 / 64.3 / 563 | 0.34 / 1.88 / 20.4 / 39.5 |
| write syscalls | 5.84 M | 456 k |
| MiB handed to the kernel | 14,891 | 1,047 |
| index build at close | — | 0.98 s |
| export | 7.5 s | 7.5 s |

What it says:

- **The hypothesis holds.** Without `obs_bssid`, the process wrote 14× less for the
  same rows on both machines, 1,047–1,051 MiB where the baseline wrote about 14.9 GiB.
  The card took the whole load with nothing dropped, where it had dropped two rows in
  three.
- **The card now sees long writes instead of scattered ones.** 90 KiB per request
  against 10.8, and a hundredth as many requests per row. At this rate the card takes
  about 12 GiB an hour for about 2.8 GiB of database, where the baseline workload
  would have asked for about 196 GiB an hour.
- **About 4.4× amplification remains.** Each commit hands the kernel about 155 KiB to
  grow the file by about 35 KiB. That is consistent with the pages a commit always
  touches: the table's last page, the tail of each node's `obs_node` run, the node and
  heartbeat rows. They are rewritten to the WAL every commit, then copied again at
  checkpoint. Longer commits (fewer rewrites of the same pages per second) and less
  frequent checkpoints (one copy of a page rewritten many times) are the levers for
  that, and are the next things to measure.
- **The slow commits are now 1 in 50 or so, and short.** p95 is under 4 ms, and p99
  and max are 125–157 ms on the card. At 11,500 rows/s a 157 ms stall is about 1,800
  rows against a 4,096-record queue, so the headroom is real but not large. These are
  likely still the automatic checkpoint.
- **Export does not get slower.** The index is built before the export either way.
  The card's 14.7 s is the same as the Optane baseline's 14.8 s for the same rows, so
  export is bound by the CPU, not the storage.

Caveats before it becomes the default:

- **The card's index build may not have written to the card.** SQLite's sort for
  `CREATE INDEX` spills to a temporary file once it outgrows its memory, and on unix
  it looks in `SQLITE_TMPDIR`, `TMPDIR`, then `/var/tmp`. On this Pi those are on the
  NVMe root, so 2.7 s is probably flattering for a box that boots from its card.
- **The build scales with the capture.** 2.7 s for 1.74 M rows is five minutes of
  this load; an hour is about twelve times the rows, and the sort is worse than
  linear.
- **A capture that never closes has no index.** A power cut, or a crash, leaves a
  database the export opens read-only and cannot index, so export would have to work
  without it or something would have to build it on the next open.
- **An export during a capture has no index to use.**
- **Peak RSS rose by about 2 MiB on both machines**, probably the build's sort. It is
  worth watching as captures get longer.

### Does export need `obs_bssid` at all?

Both export queries group by address. `SELECT_NETWORKS` uses a window partitioned by
`bssid`; the unpositioned count uses `GROUP BY bssid`. With the index, SQLite walks
the index in address order and looks every sighting up in the table by rowid, which
is one random read per row. Without it, SQLite scans the table in order and sorts in
a temporary B-tree:

| query | with `obs_bssid` | without |
| --- | --- | --- |
| networks | `SCAN o USING INDEX obs_bssid`, temp B-tree for the rest of the order | `SCAN o`, temp B-tree for the order |
| unpositioned | `SCAN observation USING INDEX obs_bssid` | `SCAN observation`, temp B-tree for the `GROUP BY` |

A smoke test on the Mac used one capture from the fixed-neighbourhood `burst` profile
(798 k sightings, 10,263 addresses), exported twice each way:

| | with `obs_bssid` | without |
| --- | --- | --- |
| `wartui export` wall time | 3.58 s, 3.12 s | 2.13 s, 2.04 s |

Without the index export was faster, not slower.

**At a full drive's scale.** One `drive` run of 350 s on the Mac with turnover at
four sightings per address and the index deferred. It produced 2,000,565 sightings of
506,360 unique addresses, with 0 idle node-windows and 0 rows dropped. Commit p50 /
p99 / max was 0.34 / 6.6 / 28.5 ms, and the index took 0.84 s to build at close.
The database held 2,014,780 observation rows (warm-up included) in 271 MiB. Exported
twice each way:

| | with `obs_bssid` | without |
| --- | --- | --- |
| wall time | 9.89 s, 8.37 s | 5.84 s, 5.40 s |
| system time | 3.10 s, 3.27 s | 0.52 s, 0.46 s |
| peak RSS | 13.5 MiB | 15.6 MiB |
| networks written | 506,746 | 506,746 |

The index costs export about 3.5 s, nearly all of it system time. That fits one
random table read per sighting, the path a card is slowest at. Sorting without it
costs about 2 MiB more memory. So `obs_bssid` does not pay for itself on either
side: capture is fourteen times cheaper without it, and export is faster. It can
leave the schema entirely rather than be deferred, which takes the build at close,
the unindexed database a crash leaves, and the unindexed export during a capture
with it. Still to confirm: the export time on the Pi's CPU and card.

**Decision.** Schema v6 drops `obs_bssid`, and opening an older capture drops it from
that file too. `--defer-bssid-index` went with it, since there is no longer an index to
defer. Every run after this change measures the store without the index, so the
"deferred" columns above are what the plain store now does, minus the build at close.

### With the hourly recapture export (`export/hourly-recapture`)

That branch replaces the per-network window query with one ordered stream,
`ORDER BY bssid, rx_at, id`, and folds it into recapture windows in Rust. Its query
wants a different index, so the index was measured again against it: the branch's own
release export over the full-drive database above (2,014,780 rows, 506,746
addresses), twice with each layout, on the Mac.

| index | plan | wall | user | sys | peak RSS |
| --- | --- | --- | --- | --- | --- |
| `obs_bssid` | index scan, temp B-tree for `rx_at, id` | 6.42 s, 4.58 s | 2.61 s, 2.70 s | 1.72 s, 1.86 s | 183 MiB |
| none | table scan, temp B-tree for the whole order | 3.29 s, 3.20 s | 2.61 s, 2.64 s | 0.35 s, 0.32 s | 185 MiB |
| `(bssid, rx_at, id)` | index scan, no sort | 4.08 s, 3.93 s | 2.16 s, 2.18 s | 1.59 s, 1.73 s | 183 MiB |

All six exports wrote the same 506,746 rows, byte for byte (same SHA-256).

- **No index is still fastest.** The composite index satisfies the whole `ORDER BY`,
  which saves about 0.45 s of sorting, and costs about 1.3 s of system time in random
  table reads, which is the access pattern a card is slowest at.
- **Its capture cost would be `obs_bssid`'s again.** Its key still leads with a random
  address, so every commit would dirty leaf pages across the index. That is inferred
  from the `obs_bssid` runs, not measured for this index. It is 50 MiB for a full drive;
  building it once over a finished capture took 1.5 s with the `sqlite3` CLI.
- **The branch's export held every submitted row in memory** for the final sort by
  window: about 183 MiB for this drive, whatever the index, against 13–16 MiB for the
  export it replaces. That is a property of the fold, not the store. The next section
  moves the sort into SQLite.

#### Sorting the submitted rows in SQLite instead of a `Vec`

The fold now writes each submitted row to a `TEMP` table on the export's connection and
reads it back `ORDER BY first_seen, bssid`. SQLite sorts within its page cache and spills
the rest to a temporary file. One `drive` run of 350 s on a Linux laptop (Core Ultra 7
258V, btrfs on NVMe) produced 2,023,542 sightings. Both builds exported it twice, with
`SQLITE_TMPDIR=/var/tmp` so the temporary files landed on the disk:

| | `Vec` | `TEMP` table |
| --- | --- | --- |
| rows written | 508,739 | 508,739 |
| wall time | 2.87 s, 2.83 s | 3.24 s, 3.32 s |
| system time | 0.22 s, 0.21 s | 0.23 s, 0.24 s |
| peak RSS | 210 MiB | 12.8 MiB |
| filesystem writes | 140 MiB | 208 MiB |

Both builds wrote the same file, byte for byte (same SHA-256).

- **Memory no longer follows the capture.** Peak RSS is back where the per-network query
  left it, and it holds one window plus SQLite's cache however long the capture is.
- **The price is temporary disk, about 68 MiB for a full drive, and 0.4 s.** The writes
  column includes the 42 MiB CSV and the spill of the fold's own `ORDER BY`, which both
  builds pay. What the `TEMP` table adds is its rows and their sort. SQLite deletes both
  files as it goes, and none were left behind.
- **On a Pi that boots from its card, that temporary file is on the card** unless
  `SQLITE_TMPDIR` or `TMPDIR` points somewhere else.

**On the CM5.** One `drive` run of 350 s on the CM5, with the database and the CSV on its
microSD card. The `TEMP` table build exported it twice with the temporary files on the
card, then twice with `SQLITE_TMPDIR=/dev/shm`:

| | temporary files on the card | in `/dev/shm` |
| --- | --- | --- |
| rows written | 509,248 | 509,248 |
| wall time | 8.53 s, 9.34 s | 9.10 s, 9.01 s |
| system time | 0.27 s, 0.37 s | 0.27 s, 0.29 s |
| peak RSS | 11.6 MiB, 12.1 MiB | 12.1 MiB, 11.1 MiB |
| filesystem writes | 257 MiB, 250 MiB | 41.6 MiB |

- **Memory holds on the Pi.** About 12 MiB, where the `Vec` would have needed about
  210 MiB for this drive.
- **Export is bound by the CPU, not the card.** It is about 2.8× the laptop's time,
  and the card and RAM runs overlap. System time barely moves either way.
- **The temporary files are about 210 MiB here**, the difference between the two
  columns. The `/dev/shm` column is only the CSV. The figure overstates the bytes,
  because `rpi-2712` kernels count whole 16 KiB pages, as the first section notes.
  It also counts writes into the page cache, not to the card. SQLite unlinks its
  temporary files as it opens them, so pages freed before writeback may never reach the
  card. That is not measured.
- **`/dev/shm` saves nothing.** It is RAM, so it holds the same temporary files in memory
  outside the process's RSS, and it was no faster. The card is the right default.

### Commit interval and background checkpoints, on the card

CM5 on the Amazon Basics microSD, ext4 `noatime`, schema v6 (no `obs_bssid`, still
`obs_node`). One `drive` run of 350 s each: a full drive of 2.01 M sightings of about
509 k addresses. Every run had 0 idle node-windows and 0 rows dropped.

| figure | inline, 100 ms | background every 1 s | inline, 1 s or 16,384 rows |
| --- | --- | --- | --- |
| rows/s | 11,529 | 11,530 | 11,536 |
| commits/s, rows/commit | 22.6, 511 | 22.6, 510 | 0.92, 12,581 |
| batch p50 / p99 / max ms | 2.3 / 127 / 159 | 2.2 / 2.4 / 337 | 48 / 252 / 267 |
| commit p50 / p95 / p99 / max ms | 0.12 / 3.8 / 125 / 157 | 0.14 / 0.18 / 0.19 / 12.7 | 1.0 / 193 / 215 / 230 |
| write syscalls | 503 k | 507 k | 227 k |
| MiB handed to the kernel | 1,125 | 1,144 | 570 |
| device writes | 13,768 | 5,936 | 5,075 |
| device MiB | 1,138 | 1,109 | 577 |
| KiB per device write | 85 | 191 | 116 |
| device MiB per MiB of database | 4.7 | 4.6 | 2.4 |
| checkpoint passes, p50 / p99 / max ms | — | 327, 52 / 253 / 311 | — |
| WAL truncations, max ms | — | 12, 269 | — |
| WAL file, largest sampled | 4.0 MiB | 56.5 MiB | 4.9 MiB |
| peak RSS | 20.4 MiB | 19.7 MiB | 23.9 MiB |
| export | 13.1 s | 12.9 s | 12.8 s |

What it says:

- **Without `obs_bssid` the card takes a full drive.** The export of 509 k addresses
  takes 13 s on the card, and peak RSS is up by about 8 MiB on the earlier fixed
  neighbourhood's, which is about what the engine's set of half a million addresses
  should cost.
- **One-second commits halve the bytes.** The pages every commit touches, the table's
  last page and each node's tail, are rewritten once a second instead of 22 times, so
  the card gets 2.4× the database's size instead of 4.7×. The slowest batch is still
  267 ms, and at 11,500 rows/s that is about 3,100 rows against a 4,096-record queue.
  Nothing dropped, but the headroom is thin.
- **The background checkpointer removed the commit stalls and kept the bytes.** Commit
  p99 fell from 125 ms to 0.19 ms. But batch max rose to 337 ms, with truncations up to
  269 ms, because `TRUNCATE` takes the writer's lock. It ran twelve times, and the
  arithmetic says why. A writer only rewinds the WAL when a checkpoint has copied every
  frame before its next transaction begins, and a 52 ms pass rarely beats commits
  45 ms apart. The WAL therefore only grew, at roughly half of the 1,144 MiB written, or
  about 100 MiB a minute, and passed 64 MiB every half-minute or so.
- **The sampled WAL size is a high-water mark.** A rewound WAL is reused from the
  start, not shrunk, so only a truncation shows up as a drop.

**Next.** Checkpoint right after a commit instead of on a timer. With one-second
commits a pass of 50–250 ms finishes well before the next transaction, so the writer
should rewind the WAL itself every commit and the truncation becomes a fallback. That
would combine the halved bytes with the vanished commit stalls, and give the queue its
headroom back.

Both have since landed. Schema v7 drops `obs_node` as well, since nothing read it.
`--checkpoint-every` now runs its pass right after a commit, at most that often, and
truncates only once a pass has caught up. The report adds two counts:

- `wal_rewinds`: passes that found the WAL rewound since the pass before.
- `checkpoints_behind`: passes that could not copy everything, busy or outrun.

### Schema v7, and checkpoints right after commits, on the card

The same CM5 and card, schema v7 (no index on `observation`), one 350 s `drive` run
each: 2.01 M sightings of about 509 k addresses, 0 idle node-windows and 0 rows
dropped every time.

| figure | inline, 100 ms | 1 s commits, pass after each | the same, queue 65,536 |
| --- | --- | --- | --- |
| commits/s, rows/commit | 22.6, 510 | 0.95, 12,196 | 0.95, 12,158 |
| batch p50 / p99 / max ms | 1.7 / 110 / 149 | 44.5 / 50.8 / 57.7 | 44.3 / 47.9 / 56.6 |
| commit p50 / p99 / max ms | 0.06 / 108 / 147 | 4.8 / 9.5 / 18.7 | 4.8 / 7.7 / 16.7 |
| write syscalls | 229 k | 160 k | 160 k |
| MiB handed to the kernel | 543 | 409 | 409 |
| device writes | 1,835 | 5,577 | 5,597 |
| device MiB | 550 | 428 | 429 |
| KiB per device write | 307 | 79 | 79 |
| device write ms, total | 23,472 | 20,559 | 27,733 |
| device MiB per MiB of database | 2.8 | 2.2 | 2.2 |
| checkpoint passes, p50 / p99 / max ms | — | 333, 31 / 45 / 66 | 334, 31 / 47 / 55 |
| WAL truncations | — | 0 | 0 |
| WAL file, largest sampled | 4.0 MiB | 0.68 MiB | 0.66 MiB |
| database at close | 196 MiB | 196 MiB | 196 MiB |
| peak RSS | 19.8 MiB | 23.8 MiB | 35.3 MiB |
| export | 13.0 s | 13.0 s | 13.0 s |

What it says:

- **Dropping `obs_node` halved the bytes on its own.** The baseline wrote 550 MiB
  where v6 wrote 1,138, and the database shrank from 242 to 196 MiB. Most of what each
  commit rewrote was the tail page of each node's run in that index.
- **Checkpointing right after a commit did what the timer could not.** Every pass
  caught up, no truncation ran, and the WAL never exceeded 0.7 MiB, about one commit.
  The writer rewound it itself.
- **The stalls are gone.** The slowest batch fell from 149 ms to 58 ms, and at
  11,500 rows/s 58 ms is about 670 rows against a 4,096-record queue, six times
  headroom where the baseline had under three. Commit p99 fell from 108 ms to 9.5 ms.
- **Bytes fell another fifth, to 2.2× the database.** The WAL and the file both get
  every page, so about 2× is the floor for WAL mode; what is left over it is the
  table's last page and the node and heartbeat rows, rewritten once a second.
- **The writes are smaller, not bigger.** 79 KiB per device write against 307: a
  checkpoint a second copies a second's pages into the file, where the inline store
  copied 4 MiB of WAL at a time. The card spent less time writing all the same
  (20.6 s against 23.5 s), and the bytes and the stalls are what cost the capture.
- **The larger queue bought nothing here and cost 11.5 MiB.** `sync_channel`
  allocates every slot up front, about 196 bytes each. With a 58 ms worst batch the
  default queue never came close to filling.
- **`wal_rewinds` undercounts.** It counted 150 of 333 passes, because it only notices
  a rewind when the next commit is smaller than the last. The WAL's size is the
  evidence.
- **Durability improves as a side effect.** Under `synchronous=NORMAL`, SQLite syncs
  the WAL when it checkpoints, not when it commits. A checkpoint a second syncs a
  second's rows, where the inline store synced every 4 MiB or so of WAL.

**Decision.** These are now the store's defaults:

- commit every second or 16,384 rows
- a queue of 16,384 records
- the background checkpoint after every commit, truncating past 64 MiB

`wartui run --commit-interval` changes the interval, and `bench --inline-checkpoint`
measures SQLite's own checkpoint for comparison. `wal_rewinds` is gone from the report:
the WAL's size says the same thing, without the undercount.

**Confirmed on the card with no flags** (commit `c6e6f67`, one 350 s `drive` run). It
matched the tuned run above, with 0 idle node-windows, 0 dropped, 0 truncations and 0
passes behind:

| figure | defaults | tuned run above (queue 4,096) |
| --- | --- | --- |
| batch p50 / p99 / max ms | 42.5 / 46.0 / 47.4 | 44.5 / 50.8 / 57.7 |
| commit p50 / p99 / max ms | 4.9 / 5.4 / 8.0 | 4.8 / 9.5 / 18.7 |
| checkpoint p50 / p99 / max ms | 31.5 / 46.0 / 58.7 | 31.2 / 45.4 / 65.6 |
| device MiB, writes | 428.7, 5,612 | 428.4, 5,577 |
| WAL file, largest sampled | 0.66 MiB | 0.68 MiB |
| peak RSS | 25.6 MiB | 23.8 MiB |
| export | 12.9 s | 13.0 s |

The queue's 1.8 MiB is in line with the estimate. The other gaps are one run apart
from each other, and are within what single runs vary by.

**Twenty nodes, same defaults** (one 350 s `burst` run, same card). The load was 4.6×
`drive`'s: 26,600 sightings a second, 53,420 rows a second. That is 9.32 M sightings
of 2.35 M addresses, about 4.7 times the drive the store is sized for. It had 0 idle
node-windows, 0 rows dropped, 0 truncations and 0 passes behind.

| figure | `burst` | `drive`, defaults |
| --- | --- | --- |
| rows/s | 53,420 | 11,531 |
| commits/s, rows/commit | 3.26, 16,374 | 0.95, 12,158 |
| batch p50 / p99 / max ms | 38 / 58 / 60 | 42.5 / 46.0 / 47.4 |
| commit p50 / p99 / max ms | 4.8 / 7.6 / 11.7 | 4.9 / 5.4 / 8.0 |
| checkpoint p50 / p99 / max ms | 37 / 54 / 62 | 31.5 / 46.0 / 58.7 |
| device MiB, writes | 1,976, 19,625 | 429, 5,612 |
| device MiB per MiB of database | 2.1 | 2.2 |
| WAL file, largest sampled | 0.85 MiB | 0.66 MiB |
| database at close | 921 MiB | 196 MiB |
| peak RSS | 59.3 MiB | 25.6 MiB |
| export | 62.5 s (2.35 M networks) | 12.9 s (509 k) |

What it says:

- **At this rate the row limit sets the pace.** Commits land every 300 ms or so, on
  the 16,384-row limit rather than the one-second interval. Every checkpoint pass,
  p99 54 ms, still finished before the next commit, so the WAL stayed at one commit's
  size.
- **The queue keeps five times headroom.** The slowest batch, 60 ms, lets about
  3,200 rows arrive against 16,384 slots. The old 4,096 would have been 78% full.
- **Nothing degraded as the file grew.** Per-minute rows/s, device MiB and commit p99
  were flat from the first minute to the sixth, while the database reached 921 MiB.
  With no index on sightings, every insert is an append.
- **Memory follows the addresses, not the store.** Peak RSS rose by about 18 bytes per
  extra address, which is the engine's set of every address it has heard. The next
  section replaces it.
- **Export time is linear in the capture.** It took 62.5 s here for 4.6× the rows that
  took 12.9 s, well past the drive the store is sized for.

### Estimating unique addresses instead of keeping them

The engine's set of every address, which drew only the view's "unique APs" and this
report's `unique_addresses`, is now a HyperLogLog of 16 KiB per kind
(`crates/wartui-core/src/distinct.rs`). `unique_addresses` from here on is an estimate,
not a count. The store is unchanged.

The same CM5 and card, one 350 s run of each profile on each build, the "before" build
being `main` at `b1ab911`:

| figure | `drive` before | `drive` after | `burst` before | `burst` after |
| --- | --- | --- | --- | --- |
| peak RSS | 25.7 MiB | 14.6 MiB | 59.2 MiB | 17.1 MiB |
| `unique_addresses` | 508,840 | 512,533 | 2,351,377 | 2,360,882 |
| rows dropped, idle node-windows | 0, 0 | 0, 0 | 0, 0 | 0, 0 |
| batch p99 ms | 45.9 | 46.5 | 57.3 | 55.2 |
| commit p99 ms | 7.5 | 8.6 | 9.1 | 8.9 |
| device MiB, writes | 428.7, 5,613 | 428.4, 5,582 | 1,975.8, 19,601 | 1,976.4, 19,619 |
| KiB per device write | 78.2 | 78.6 | 103.2 | 103.2 |
| WAL file, largest sampled | 0.66 MiB | 0.67 MiB | 0.85 MiB | 0.85 MiB |

What it says:

- **The set cost more at its peak than per address.** It fell by 11.1 MiB on `drive`
  and 42.1 MiB on `burst`. That is consistent with the set's last doubling holding the
  old table and the new one at once: one byte of control and six of key per slot, in a
  power-of-two table kept seven-eighths full, comes to about 10.5 MiB at 509 k
  addresses and 42 MiB at 2.35 M.
- **Memory no longer follows the addresses.** Twenty nodes and 4.6 times the
  addresses cost 2.5 MiB more than ten, which is the larger fleet and the queue.
- **The estimate is within 1%.** +0.7% and +0.4% on the old exact counts, though
  from separate runs, so each gap holds the run's variation as well as the estimate's.
- **The store did not notice.** Bytes and requests to the card agree within 0.6%, the
  WAL peaked at the same size, and batch and commit p99 moved by what single runs vary
  by.
