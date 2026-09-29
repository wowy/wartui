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

Not yet taken. Scanner build: `--features esp32c6,xiao-external-antenna`, which prints
`antenna: external (U.FL)` at boot.

## Decision

Pending the drive. The rule set in advance: propose extended scanning for the node if
extended-only addresses are at least 5% of the distinct addresses on the drive, or if
X1 matches L's legacy yield within noise, which would make switching to X1 free.
The room shows the second condition holding for X1, and none of the gain the first
condition asks for.
