# Extended BLE scanning — what the legacy scan misses

Measured on 2026-09-28. The node's Bluetooth scan uses the legacy commands
(`HCI_LE_Set_Scan_Parameters`/`_Enable`) and reads only the LE Advertising Report,
subevent 0x02. That scan can't hear an advertiser that uses extended advertising
alone: an `AUX_ADV_IND` chain on 1M, or anything on the Coded PHY. The question was
how many such advertisers there are, and whether that's worth a change to the
scan path. This was benched before anything was committed to production.

## Method

`firmware/node/examples/ext_scan_bench.rs` runs on the XIAO C6 `9A:24`, with no Wi-Fi and
no ESP-NOW, so Bluetooth has the radio to itself as it does on the fleet's Bluetooth
node. It cycles through three modes, 60 s each, with an `HCI_Reset` between them
because a controller runs one command family per reset:

- **L**: the production scan. Legacy, 1M.
- **X1**: extended scan, 1M only.
- **XC**: extended scan, 1M and Coded.

Every mode runs 500 ms scans back to back at interval = window = 30 ms, as
`firmware/node/src/ble.rs` does. The bench keeps a session-wide address set and marks
each address with the modes that heard it and whether any report of it was legacy.
`ext_only_never_legacy` counts addresses heard only by X1/XC and **never as a
legacy PDU**. That count is the coverage the legacy scan is missing.

`firmware/node/examples/ext_adv_beacon.rs` is the positive control. It runs on the C5
`0A:CC` and advertises two extended-only, non-connectable sets through raw HCI:
`C0:DE:BE:AC:00:01` on 1M primary / 2M secondary, and `…:00:02` on Coded / Coded,
both carrying manufacturer data 0xFFFF.

## The controller supports it

Both boards accepted every command with status 0x00. The C6 lists
`LE_Set_Extended_Scan_Parameters`/`_Enable` and reports the LE feature bits
`0x00001b900f017fff`, which include Extended Advertising (bit 12) and Coded PHY
(bit 11). The C5 advertised both sets at once. That needed
`Config::with_multi_adv_instances(2)`, because esp-radio defaults to one set.

## The control passes

One 3-minute cycle with the beacon on:

| mode | control 1M (`…:01`) | control Coded (`…:02`) |
| --- | --- | --- |
| L | never | never |
| X1 | 534 reports, `legacy=false`, PHY 1/2, mfgr 0xFFFF | never |
| XC | 278 reports | 254 reports, PHY 3/3 |

The session's `ext_only_never_legacy` was 2, and those two were the control
addresses. So the scan hears an extended-only advertiser on either PHY, and the
legacy scan doesn't.

## The room has none

These runs, and the control above, were on the XIAO's **onboard ceramic antenna**. The
first build left out `xiao-external-antenna`, so the RF switch stayed at its default.
That doesn't matter for comparing L, X1 and XC, which all ran on the same antenna in
the same runs, but the absolute counts are probably below what the fleet's Bluetooth
node hears on the U.FL antenna. The drive build sets the switch.

Two 6-minute runs with the beacon off. The second completed both cycles. The first
crashed in its second L window; see below.

| run, cycle | L distinct | X1 distinct | XC distinct | XC reports vs L |
| --- | --- | --- | --- | --- |
| 1, 1 | 46 | 53 | 54 | 53% |
| 2, 1 | 51 | 49 | 47 | 51% |
| 2, 2 | 52 | 46 | 44 | 56% |

**`ext_only_never_legacy` was 0 in every session.** Every address the extended modes
heard was also heard, at least once, as a legacy PDU. `ext_only` was 2–12: legacy
advertisers that happened to be heard in X1/XC but not in L, which is the usual churn
of rotating random addresses and marginal RSSI, not a coverage gap. No report was
anonymous, incomplete or truncated, and no window came near the 80-slot `REPORTS`
buffer.

X1 hears legacy advertisers about as well as L: within the ±5 spread between windows
of the same mode. XC takes roughly half the reports L does, because the controller
splits listening time between the two PHYs. Its distinct count holds up for now,
because each advertiser repeats often enough to be caught in half the time.

## An intermittent crash in the bench

In one of four runs longer than 2 minutes, the scanner panicked a few seconds into the
L window that follows an XC window: `Load access fault` in esp-rtos's
`RunQueue::mark_task_ready`, on a task pointer that is garbage (`0x7e2eb040`). These
didn't crash:

- another full 6.5-minute run of the same build
- 2-minute runs of 10 s modes cycling XC→L, X1→L, XC→XC and L→L
- 6 minutes of L→L

What differs is the one thing the bench does and production doesn't: resetting the
controller out of an extended (Coded) scan and back into a legacy one. The trigger isn't
pinned down, and the fault is inside the controller's OS glue, not in code here.
**Production is not exposed to it** as things stand: it resets once at boot and only
ever scans legacy. If the node moves to extended scanning, it should be one family from
boot with no family switch, and a soak test of that build should come first.

## Drive

84 minutes (28 cycles), on 2026-09-28, on the U.FL antenna (`antenna: external (U.FL)` at
boot). Scanner build: `--features esp32c6,xiao-external-antenna`.

### Only the first 18 minutes count

The session address set holds 1,024 entries, which is enough for a room and not for a road.
It filled during cycle 6. From then on, any new address was only counted as overflow, which
reached 129,142 reports by cycle 27. Each window's `distinct`, `legacy` and `ext_*` counts are
read from that same set. So after cycle 6 they only count addresses already in it, and the
long run of `distinct=3` windows from cycle 7 onward is an artifact of the bench, not an empty
road. What holds for the whole drive is the report totals and the restart record. Everything
about distinct addresses comes from cycles 1–6.

### The gap is under 1%, all on 1M

| session | distinct | heard only by extended | never a legacy PDU | of which Coded |
| --- | --- | --- | --- | --- |
| after cycle 5 | 831 | 339 | **4 (0.5%)** | 0 |
| after cycle 6 (set full) | 1,024 | 466 | **8 (0.8%)** | 0 |

Every advertiser the legacy scan missed was on the 1M PHY. **Nothing was heard on Coded
that wasn't also heard on 1M**, and no report was anonymous or truncated. Those
advertisers only turned up in two windows (X1 in cycles 5 and 6, and XC in cycle 5), 3–5 at
a time, and each came with reports marked incomplete, as an `AUX` chain would.
`heard only by extended` is again mostly legacy advertisers the L windows happened not to
catch: at speed, successive windows are in different places.

### Legacy yield

Report totals over the drive: L 123,731 (28 windows), X1 133,104 (28), XC 62,518 (27). So X1
takes in as much as L does, and XC takes in half, the same as in the room. Distinct counts in
cycles 1–4 are within the spread between windows of the same mode (L 104–209, X1 90–158). At
speed, each window covers a different stretch of road, so the drive can't separate these more
finely.

### No crash

No panic and no restart in 84 minutes: 27 transitions from XC back to L on the external
antenna. The crash in the room is still a single occurrence.

## Decision

Measured against the rule set in advance:

- **Extended-only share ≥ 5% on the drive: no.** It was 0.5–0.8% over the 18 minutes the
  bench could track, an order of magnitude short, and none of it was on Coded. The other 66
  minutes weren't tracked, so they can't confirm or contradict that.
- **X1 matches L's legacy yield: yes**, both in the room and in the drive's report totals.
  Switching the node to an extended scan on 1M only would cost nothing measurable in
  legacy coverage and would gain those 0.5–0.8%. Coded costs half the reports and gained
  nothing.

**Not adopted, pending the side-by-side A/B below.** A gain of under 1% doesn't justify changing the scan path. The change
would bring a new report type through `BlePending`, a rule for anonymous addresses, and
a controller path that crashed once on this bench. The node keeps the legacy scan.
Worth revisiting if the fleet's surroundings change, since extended-only advertisers
are what newer devices tend to become. The examples and the `wartui-proto::hci`
extended pieces stay in the tree, so the bench can be rerun as it is. If it's rerun on
a drive, the session set needs to be larger than 1,024 entries, and the per-window
counts need to stop depending on it.

## Side by side: L and X1 at the same time

The drive couldn't show whether X1 loses legacy advertisers that L hears, because
the modes took turns and each window heard a different stretch of road. So the
bench now takes its modes at build time (`BENCH_MODES=L` or `X1`). Two XIAO C6
boards, `75:40` and `9A:24`, run it at once on their U.FL antennas. Each board logs
every address the first time it's heard, and `tools/ble_ab_compare.py` compares
the two boards' address sets over the time both logs cover. The session set is now
an 8,192-slot hash table, and the per-window counts come from their own set, so
neither saturates the way the drive did.

### Room, 2026-09-28: the board matters, the mode doesn't

Two 5-minute halves, with the modes swapped between boards at half time so any
difference between the boards cancels out:

| | `9A:24` distinct / window | `9A:24` reports / window | `75:40` distinct / window | `75:40` reports / window |
| --- | --- | --- | --- | --- |
| X1 | 50–61, median 54 | 8.5k–9.5k | 36–40, median 36 | 7.4k–7.6k |
| L | 49–54, median 53 | 8.5k–8.8k | 36–42 (3 windows) | 7.5k–7.7k |

`9A:24` hears about 40% more distinct advertisers than `75:40`, whichever mode it runs.
The likely cause is the antenna or its cable. On either board, X1 and L agree to within
a few percent. In the second half both boards booted together. Every address `75:40`
heard on X1 was also heard by `9A:24` on L, and `9A:24` heard 16 more, which is the
board gap again. Neither board heard an address that was never sent as a legacy PDU.

**A drive has to swap the boards halfway too.** A 40% gap between the boards would
swamp any difference between the modes.

### More bench crashes

In the first half, `75:40` restarted about 20 times in its first ~110 s of scanning
**on L**, then ran cleanly. The panics were all memory corruption:

- the heap allocator catching a double free (`Freed node … aliases existing hole`)
- `ASSERT r_ble_lll_scan_rx_process:1646` and `ASSERT r_ble_lll_mmgmt_free:1811`
  inside the controller
- load access faults in `r_ble_lll_get_rxed_buffer`

It didn't happen again with X1 on the same board in the second half, or with L on
that board alone for 2 minutes afterwards. Both bench crashes so far happened in L
windows. That is the production mode, but production has run for hours on these
boards without a crash. What the bench adds is heavy `println!` from the collect loop
and an `HCI_Reset` every window. The cause is unknown.

