#!/usr/bin/env python3
"""Read-only survey of a wartui capture database, and optionally its --log-file.

Prints facts, section by section, then a FLAGS list. Each flag carries the SQL that reproduces
it. It interprets nothing beyond thresholds: the reviewer reads the source to say why.

The file is opened read-only (`mode=ro`), so a capture that is still being written is safe to
survey. Every section tolerates a missing table or column, because captures come from many
builds and the schema changes shape freely before 1.0.

Usage: python3 -I survey.py CAPTURE.db [--log wartui.log] [--node 59:50]
"""

import argparse
import math
import re
import sqlite3
import statistics
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone

FLAGS = []  # (severity, area, text, sql)

# crates/wartui-proto/src/plan.rs SCAN_CHANNELS at the time of writing; used only when a capture
# predates the `capture.scan_channels` kv row. Indices are the wire format, so order matters.
SCAN_CHANNELS_FALLBACK = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 36, 40, 44, 48, 52, 56,
                          60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 149,
                          153, 157, 161, 165, 169, 173, 177]


def flag(sev, area, text, sql=""):
    FLAGS.append((sev, area, text, sql.strip()))


# ---------------------------------------------------------------------------- helpers


def short(mac):
    """A MAC by its last two octets, as the fleet table and docs name boards."""
    if mac is None:
        return "?"
    if isinstance(mac, (bytes, bytearray)) and len(mac) == 6:
        return f"{mac[4]:02X}:{mac[5]:02X}"
    return str(mac)


def full(mac):
    if isinstance(mac, (bytes, bytearray)):
        return ":".join(f"{b:02X}" for b in mac)
    return str(mac)


def utc(ms):
    if ms is None:
        return "-"
    try:
        return datetime.fromtimestamp(ms / 1000, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    except (OverflowError, OSError, ValueError):
        return f"{ms} ms"


START_MS = None


def rel(ms):
    """Time since the capture began, as +HhMMmSSs."""
    if ms is None or START_MS is None:
        return "?"
    s = (ms - START_MS) / 1000
    sign = "-" if s < 0 else "+"
    s = abs(int(s))
    return f"{sign}{s // 3600}h{s % 3600 // 60:02d}m{s % 60:02d}s"


def dur(ms):
    s = int(ms / 1000)
    if s < 120:
        return f"{s}s"
    if s < 7200:
        return f"{s // 60}m{s % 60:02d}s"
    return f"{s // 3600}h{s % 3600 // 60:02d}m"


def pct(part, whole):
    return f"{part * 100 / whole:.1f}%" if whole else "n/a"


def pctile(values, p):
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))]


def spread(values, unit=""):
    if not values:
        return "n/a"
    return (
        f"min {min(values)}{unit}  p50 {pctile(values, 50)}{unit}  "
        f"p95 {pctile(values, 95)}{unit}  max {max(values)}{unit}"
    )


def section(title):
    print(f"\n== {title} ==")


class Db:
    def __init__(self, path):
        uri = f"file:{path}?mode=ro"
        self.conn = sqlite3.connect(uri, uri=True)
        self.tables = {
            r[0] for r in self.conn.execute("SELECT name FROM sqlite_master WHERE type='table'")
        }
        self._cols = {}

    def cols(self, table):
        if table not in self._cols:
            self._cols[table] = (
                {r[1] for r in self.conn.execute(f"PRAGMA table_info({table})")}
                if table in self.tables
                else set()
            )
        return self._cols[table]

    def has(self, table, *columns):
        return table in self.tables and all(c in self.cols(table) for c in columns)

    def q(self, sql, args=()):
        return self.conn.execute(sql, args).fetchall()

    def one(self, sql, args=()):
        row = self.conn.execute(sql, args).fetchone()
        return row[0] if row else None


def absent(what):
    print(f"  n/a ({what} absent)")


def since_boot_delta(cur, prev, rebooted):
    """The increase of a since-boot counter, as the engine's advance_since_boot takes it."""
    if prev is None:
        return 0
    if rebooted or cur < prev:
        return cur
    return cur - prev


# ---------------------------------------------------------------------------- sections


def provenance(db, ctx):
    global START_MS
    section("capture")
    if not db.has("capture", "started_at"):
        absent("capture table")
        return
    cols = db.cols("capture")
    row = dict(zip(sorted(cols), db.q(f"SELECT {', '.join(sorted(cols))} FROM capture")[0]))
    START_MS = row.get("started_at")
    end = row.get("ended_at")
    ctx["start"], ctx["end"] = START_MS, end
    print(f"  started   {utc(START_MS)}")
    if end:
        print(f"  ended     {utc(end)}  ({dur(end - START_MS)})")
    else:
        print("  ended     NULL (still running, or the host died before shutdown)")
        flag(
            "INFO",
            "capture",
            "capture.ended_at is NULL: still running, or the host exited without a clean shutdown",
            "SELECT started_at, ended_at FROM capture",
        )
    print(
        f"  bridge    {full(row.get('bridge_mac'))}  chip {row.get('bridge_chip')}  "
        f"fw {row.get('bridge_fw')}"
    )
    print(f"  pool      {row.get('channel_pool')}   simulated {row.get('simulated')}")
    if row.get("notes"):
        print(f"  notes     {row['notes']}")
    if row.get("simulated"):
        flag("WARN", "capture", "simulated=1: test data (--sim or --lat/--lon); never uploaded")

    if "kv" not in db.tables:
        ctx["scan"] = SCAN_CHANNELS_FALLBACK
        return
    kv = dict(db.q("SELECT k, v FROM kv"))
    ctx["kv"] = kv
    for k in sorted(kv):
        if not k.startswith("capture.settings.") and k != "capture.scan_channels":
            print(f"  kv        {k} = {kv[k]}")
    if "capture.scan_channels" in kv:
        ctx["scan"] = [int(c) for c in kv["capture.scan_channels"].split(",") if c]
    else:
        ctx["scan"] = SCAN_CHANNELS_FALLBACK
        print("  kv        no capture.scan_channels or settings rows (older build); channel "
              "checks assume this tree's SCAN_CHANNELS")

    settings = sorted(
        (k, v) for k, v in kv.items() if k.startswith("capture.settings.") and k.endswith(
            tuple("0123456789")
        )
    )
    initial = kv.get("capture.settings.initial")
    if initial:
        s = dict(p.split("=", 1) for p in initial.split(";") if "=" in p)
        ctx["settings"] = s
        print(f"  settings  {initial}")
    prev = initial
    for k, v in settings:
        if v != prev:
            before = dict(p.split("=", 1) for p in (prev or "").split(";") if "=" in p)
            after = dict(p.split("=", 1) for p in v.split(";") if "=" in p)
            changed = {
                key: (before.get(key), after.get(key))
                for key in after
                if key != "at_ms" and before.get(key) != after.get(key)
            }
            at = int(after.get("at_ms", 0) or 0)
            if changed:
                print(f"  change    {rel(at)}  {changed}")
                ctx.setdefault("settings_changes", []).append((at, changed))
            prev = v
        ctx["settings_final"] = dict(p.split("=", 1) for p in v.split(";") if "=" in p)
    s = ctx.get("settings", {})
    if s.get("record_raw") == "false":
        flag(
            "INFO",
            "capture",
            "record_raw=false: raw_frame is empty, so frame-level questions cannot be answered "
            "from this capture",
        )


def nodes(db, ctx):
    section("nodes")
    if not db.has("node", "mac"):
        absent("node table")
        return
    rows = db.q("SELECT mac, label, first_seen, last_seen, capabilities FROM node ORDER BY mac")
    ctx["nodes"] = {r[0]: r for r in rows}
    for mac, label, first, last, caps in rows:
        if ctx["only"] and short(mac) != ctx["only"]:
            continue
        print(
            f"  {short(mac)}  {full(mac)}  {caps or 'no capabilities'}  "
            f"seen {rel(first)} .. {rel(last)}" + (f"  label {label}" if label else "")
        )
        if caps is None:
            flag(
                "INFO",
                "nodes",
                f"{short(mac)} never reported capabilities: heard only by sighting, never "
                "assignable",
                f"SELECT * FROM node WHERE hex(mac)='{mac.hex().upper()}'",
            )
    pref = ctx.get("settings", {}).get("preferred_ble")
    if pref and pref != "none":
        present = any(full(m) == pref.upper() for m in ctx["nodes"])
        if not present:
            flag(
                "INFO",
                "config",
                f"preferred_ble={pref} (the remembered Bluetooth node) never appeared, so no "
                "node scanned Bluetooth",
                "SELECT k, v FROM kv WHERE k LIKE 'capture.settings%'",
            )
    if len(rows) > 20:
        print(f"  {len(rows)} nodes: more than the supported twenty")


def heartbeats(db, ctx):
    section("heartbeats")
    if not db.has("heartbeat", "node_mac", "beat", "counter"):
        absent("heartbeat table or beat column")
        return
    cols = db.cols("heartbeat")
    want = ["node_mac", "id", "rx_at", "counter", "epoch", "rssi", "beat"]
    for c in ("live", "wifi_dropped", "ble_dropped", "admin_sent", "admin_acked",
              "admin_latency_us"):
        want.append(c if c in cols else "NULL")
    rows = db.q(f"SELECT {', '.join(want)} FROM heartbeat ORDER BY node_mac, id")
    per = defaultdict(list)
    for r in rows:
        per[r[0]].append(r)
    ctx["hb"] = per
    timeout = int(ctx.get("settings", {}).get("topology_timeout_ms", 60000))
    ble_nodes = ctx.get("ble_nodes", set())
    miss_by_job = Counter()
    beats_by_job = Counter()
    total_reversals = 0
    for mac, hs in per.items():
        if ctx["only"] and short(mac) != ctx["only"]:
            continue
        heard = missed = reboots = replayed = dups = 0
        wifi_ref = ble_ref = 0
        silences = []
        reversals = 0
        prev = None
        intervals = []
        for r in hs:
            (_, rid, rx, counter, epoch, rssi, beat, live, wd, bd, *_rest) = r
            replayed += live == 0
            if prev is not None:
                p_rx, p_counter, p_beat, p_live, p_wd, p_bd = (
                    prev[2], prev[3], prev[6], prev[7], prev[8], prev[9])
                rebooted = ((counter - p_counter) % 2**32 >= 2**31
                            or (beat - p_beat) % 2**16 >= 2**15)
                if rebooted:
                    reboots += 1
                    ctx.setdefault("restarts", []).append((rx, mac, p_counter, counter, p_beat,
                                                           beat, p_rx))
                    gap_h, gap_m = 1, max(0, beat - 1)
                else:
                    d = (beat - p_beat) % 2**16
                    gap_h, gap_m = (0, 0) if d == 0 else (1, d - 1)
                    dups += d == 0
                heard += gap_h
                if p_live != 0:  # a gap after a replayed beat spans time no host read
                    missed += gap_m
                if wd is not None and p_wd is not None:
                    wifi_ref += since_boot_delta(wd, p_wd, rebooted)
                    ble_ref += since_boot_delta(bd, p_bd, rebooted)
                dt = rx - p_rx
                if dt < 0:
                    reversals += 1
                else:
                    intervals.append(dt)
                if dt > timeout:
                    silences.append((p_rx, rx))
            prev = r
        job = "ble" if mac in ble_nodes else "wifi"
        miss_by_job[job] += missed
        beats_by_job[job] += heard + missed
        rssis = [r[5] for r in hs if r[5] is not None]
        half = len(rssis) // 2
        trend = ""
        if half > 10:
            trend = (f"  (first half p50 {statistics.median(rssis[:half])}, "
                     f"second half p50 {statistics.median(rssis[half:])})")
        print(
            f"  {short(mac)} [{job}]  rows {len(hs)}  missed {missed}/{heard + missed} "
            f"({pct(missed, heard + missed)})  restarts {reboots}  replayed {replayed}  "
            f"repeats {dups}"
        )
        if intervals:
            print(f"        interval ms  {spread(intervals)}")
        print(f"        link rssi    {spread(rssis, ' dBm')}{trend}")
        if wifi_ref or ble_ref:
            print(f"        ring refused wifi {wifi_ref}  ble {ble_ref}  (ring pressure, not loss)")
        epochs = Counter(r[4] for r in hs)
        print(f"        epochs held  {dict(sorted(epochs.items()))}")
        for a, b in silences[:5]:
            print(f"        silent       {rel(a)} -> {rel(b)}  ({dur(b - a)})")
        if silences:
            flag(
                "WARN",
                "node",
                f"{short(mac)} went {len(silences)} time(s) without a heartbeat for longer than "
                f"topology_timeout ({timeout} ms); longest {dur(max(b - a for a, b in silences))}",
                f"SELECT id, rx_at, rx_at - LAG(rx_at) OVER (ORDER BY id) AS gap FROM heartbeat "
                f"WHERE hex(node_mac)='{mac.hex().upper()}' ORDER BY gap DESC LIMIT 10",
            )
        rate = missed / (heard + missed) if heard + missed else 0
        if rate > 0.02:
            flag(
                "WARN",
                "loss",
                f"{short(mac)} missed {missed} of {heard + missed} heartbeats ({rate:.1%})",
                "-- wartui analyze --db <db> --heartbeat-windows",
            )
        if reversals:
            total_reversals += reversals
        # Miss rate against the link RSSI of the beat that arrived after each gap.
        buckets = defaultdict(lambda: [0, 0])
        for a, b in zip(hs, hs[1:]):
            if b[5] is None or a[7] == 0:
                continue
            d = (b[6] - a[6]) % 2**16
            if d == 0 or d >= 2**15:
                continue
            k = int(b[5] // 5 * 5)
            buckets[k][0] += d - 1
            buckets[k][1] += d
        if missed and buckets:
            print("        miss by rssi " + "  ".join(
                f"{k}..{k + 4}:{m}/{n}" for k, (m, n) in sorted(buckets.items(), reverse=True)
                if n >= 20))
        if missed and ctx.get("start"):
            per20 = Counter()
            for a, b in zip(hs, hs[1:]):
                d = (b[6] - a[6]) % 2**16
                if 1 < d < 2**15 and a[7] != 0:
                    per20[(b[2] - ctx["start"]) // 1_200_000] += d - 1
            if per20:
                print("        miss per 20m " + " ".join(
                    str(per20.get(i, 0)) for i in range(max(per20) + 1)))
        if missed >= 20:
            # Spacing in beats between consecutive missed beats. Random loss makes 1 the most
            # common spacing; a mode of 2 or more means something periodic hits the beat.
            pos = 0
            misses = []
            for a, b in zip(hs, hs[1:]):
                d = (b[6] - a[6]) % 2**16
                if d >= 2**15 or (b[3] - a[3]) % 2**32 >= 2**31:
                    pos += 10**6  # a restart; never pair misses across it
                    continue
                if d > 1 and a[7] != 0:
                    misses.extend(range(pos + 1, pos + d))
                pos += d
            spacing = Counter(min(y - x, 7) for x, y in zip(misses, misses[1:]) if y - x < 10**5)
            if spacing:
                n = sum(spacing.values())
                print("        miss spacing " + "  ".join(
                    f"{'7+' if k == 7 else k}:{spacing[k]}" for k in range(1, 8) if spacing[k])
                      + "  (beats between consecutive misses)")
                # Independent loss at rate p gives P(k) = p(1-p)^(k-1), falling with k, so any
                # short spacing that outnumbers spacing 1 is a period, not chance.
                mode = max(range(1, 7), key=lambda k: spacing.get(k, 0))
                count = spacing.get(mode, 0)
                if mode >= 2 and count >= 10 and count > spacing.get(1, 0):
                    flag("WARN", "node",
                         f"{short(mac)} misses heartbeats periodically: {count} of {n} gaps "
                         f"between misses are exactly {mode} beats, against {spacing.get(1, 0)} "
                         "back-to-back (independent loss would make 1 the most common)",
                         f"-- beat numbers of misses for {short(mac)}: walk heartbeat.beat by id")
        admin = [r for r in hs if r[10]]
        if admin:
            acked = sum(1 for r in admin if r[11] == 1)
            lat = [r[12] for r in admin if r[12] is not None]
            print(f"        admin windows sent {len(admin)}  acked {acked}  latency "
                  f"{spread(lat, ' us')}")
            if acked < len(admin):
                flag("WARN", "admin",
                     f"{short(mac)}: {len(admin) - acked} of {len(admin)} admin frames sent on "
                     "a heartbeat window were not acked",
                     "SELECT * FROM heartbeat WHERE admin_sent=1 AND admin_acked IS NOT 1")
    restarts = sorted(ctx.get("restarts", []))
    groups = []
    for r in restarts:
        if groups and r[0] - groups[-1][-1][0] <= 30_000:
            groups[-1].append(r)
        else:
            groups.append([r])
    fleet = len(per)
    for g in groups:
        macs = sorted({short(r[1]) for r in g})
        at = g[0][0]
        silent = max(r[0] - r[6] for r in g)
        detail = ", ".join(f"{short(r[1])} counter {r[2]}->{r[3]} beat {r[4]}->{r[5]}" for r in g)
        print(f"  restart near {rel(at)}: {', '.join(macs)}  (longest silence {dur(silent)})")
        who = (f"all {fleet} nodes restarted together" if len(macs) == fleet and fleet > 1
               else f"{', '.join(macs)} restarted")
        flag("WARN", "node",
             f"{who} near {rel(at)} ({utc(at)}); longest heartbeat silence {dur(silent)}"
             + ("; a shared cause (power, bridge, host) is more likely than node firmware"
                if len(macs) > 1 else ""),
             f"-- {detail}\nSELECT id, node_mac, rx_at, counter, beat FROM heartbeat WHERE rx_at "
             f"BETWEEN {at - 60000} AND {at + 30000} ORDER BY id")
    if "admin_sent" in cols and rows and not any(r[10] for r in rows) and db.has("assignment"):
        if db.one("SELECT COUNT(*) FROM assignment"):
            flag("INFO", "host",
                 "heartbeat.admin_sent/admin_acked/admin_latency_us are never set although "
                 "assignments went out: the columns are declared but not written",
                 "SELECT admin_sent, admin_acked, COUNT(*) FROM heartbeat GROUP BY 1, 2")
    end = ctx.get("end")
    lasts = [hs[-1][2] for hs in per.values() if hs]
    if end and len(lasts) > 1 and end - max(lasts) > 30_000 and max(lasts) - min(lasts) < 15_000:
        print(f"  every node fell silent within {dur(max(lasts) - min(lasts))} of each other, "
              f"{dur(end - max(lasts))} before the capture ended (fleet power off before quit?)")
    if beats_by_job["ble"] and beats_by_job["wifi"]:
        print(
            f"  by job: wifi-sweeping missed {pct(miss_by_job['wifi'], beats_by_job['wifi'])}, "
            f"bluetooth missed {pct(miss_by_job['ble'], beats_by_job['ble'])}"
        )
    if total_reversals:
        flag("WARN", "clock",
             f"heartbeat rx_at stepped backwards {total_reversals} time(s) in arrival order: the "
             "host wall clock moved (NTP/GPS time set?)",
             "SELECT id, rx_at, rx_at - LAG(rx_at) OVER (ORDER BY id) d FROM heartbeat "
             "ORDER BY d LIMIT 10")


def assignments(db, ctx):
    section("assignments")
    if not db.has("assignment", "node_mac", "counter", "outcome"):
        absent("assignment table")
        return
    rows = db.q(
        "SELECT node_mac, counter, wire_version, channels, ble, created_at, delivered_at, outcome, "
        "latency_us FROM assignment ORDER BY id"
    )
    outcomes = Counter(r[7] for r in rows)
    lat = [r[8] for r in rows if r[8] is not None]
    print(f"  rows {len(rows)}  outcomes {dict(outcomes)}  latency {spread(lat, ' us')}")
    ble = {r[0] for r in rows if r[4]}
    ctx["ble_nodes"] = ble
    if ble:
        print(f"  bluetooth job given to {', '.join(short(m) for m in ble)}")
    if ctx.get("start") and ctx.get("end") and rows:
        hours = (ctx["end"] - ctx["start"]) / 3_600_000
        if hours > 0:
            print(f"  {len(rows) / hours:.1f} assignment rows per hour")
    bad = [r for r in rows if r[7] != "acked"]
    if bad:
        flag("WARN", "admin",
             f"{len(bad)} assignment(s) not acked: {dict(Counter(r[7] for r in bad))}",
             "SELECT * FROM assignment WHERE outcome IS NOT 'acked'")
    counters = sorted(r[1] for r in rows)
    if counters:
        missing = sorted(set(range(counters[0], counters[-1] + 1)) - set(counters))
        if missing:
            print(f"  epochs with no row: {missing[:20]}{' ...' if len(missing) > 20 else ''}")
    scan = ctx.get("scan")
    assigned = defaultdict(set)
    for r in rows:
        if scan:
            assigned[r[0]] |= {scan[i] for i in range(len(scan)) if r[3] >> i & 1}
        print(
            f"  {short(r[0])}  epoch {r[1]} (wire {r[2]})  {r[7]}  at {rel(r[5])}  "
            f"{'BLE' if r[4] else ''}"
            + (f"  {len([i for i in range(len(scan)) if r[3] >> i & 1])} channels" if scan else "")
        )
    ctx["assigned"] = assigned
    if scan:
        print("  final share per node (* = control channel 6, where heartbeats and admin go):")
        last = {}
        for r in rows:
            last[r[0]] = r
        for mac, r in sorted(last.items()):
            chans = [scan[i] for i in range(len(scan)) if r[3] >> i & 1]
            print(f"    {short(mac)}  " + " ".join(f"{c}*" if c == 6 else str(c) for c in chans)
                  + ("  BLE" if r[4] else ""))

    # Adoption: a heartbeat well after an acked assignment should carry its wire epoch.
    hb = ctx.get("hb", {})
    grace = 15_000
    for mac in {r[0] for r in rows}:
        acked = sorted((r[6], r[2]) for r in rows if r[0] == mac and r[7] == "acked" and r[6])
        if not acked:
            continue
        mismatched = 0
        first = None
        restarted_at = [r[0] for r in ctx.get("restarts", []) if r[1] == mac]
        for h in hb.get(mac, []):
            if any(0 <= h[2] - t <= 30_000 for t in restarted_at):
                continue  # a restarted node holds no epoch until the host re-sends one
            settled = [i for i, (t, _) in enumerate(acked) if t <= h[2] - grace]
            if not settled:
                continue
            # The epoch settled by now, or any acked after it: a newer one may have just landed.
            fine = {w for t, w in acked[settled[-1]:] if t <= h[2]}
            if h[4] not in fine:
                mismatched += 1
                first = first or h[2]
        if mismatched:
            flag("WARN", "admin",
                 f"{short(mac)}: {mismatched} heartbeats >15 s after an acked assignment report a "
                 f"different epoch (first at {rel(first)}): acked but not adopted",
                 f"SELECT id, rx_at, epoch FROM heartbeat WHERE hex(node_mac)='{mac.hex().upper()}'")


def batch_gaps(db, ctx):
    section("batch gaps")
    if not db.has("batch_gap", "node_mac", "lost"):
        absent("batch_gap table")
        return
    rows = db.q("SELECT node_mac, rx_at, lost FROM batch_gap ORDER BY id")
    if not rows:
        print("  none recorded")
        return
    per = defaultdict(list)
    for mac, rx, lost in rows:
        per[mac].append((rx, lost))
    for mac, gaps in sorted(per.items()):
        if ctx["only"] and short(mac) != ctx["only"]:
            continue
        total = sum(lost for _, lost in gaps)
        buckets = Counter((rx - (ctx.get("start") or 0)) // 300_000 for rx, _ in gaps)
        worst = buckets.most_common(3)
        print(
            f"  {short(mac)}  gaps {len(gaps)}  lost {total}  largest {max(l for _, l in gaps)}  "
            f"busiest 5-min windows "
            + ", ".join(f"+{b * 5}m:{n}" for b, n in worst)
        )
    total = sum(r[2] for r in rows)
    flag("INFO" if total < 50 else "WARN", "loss",
         f"{total} sighting batches lost between node and host across {len(per)} node(s)",
         "SELECT hex(node_mac), COUNT(*), SUM(lost) FROM batch_gap GROUP BY 1")


def bridge(db, ctx):
    section("bridge status")
    if not db.has("bridge_status", "uptime_ms"):
        absent("bridge_status table")
        return
    rows = db.q(
        "SELECT id, rx_at, peer_count, rx_count, dropped_tx, uptime_ms, host_frames "
        "FROM bridge_status ORDER BY id"
    )
    if not rows:
        print("  no status replies")
        flag("WARN", "bridge", "no bridge_status rows: the bridge never answered a status poll")
        return
    reboots = []
    received = dropped = 0
    gaps = []
    for prev, cur in zip(rows, rows[1:]):
        restarted = cur[5] < prev[5]
        if restarted:
            reboots.append(cur[1])
        received += since_boot_delta(cur[3], prev[3], restarted)
        dropped += since_boot_delta(cur[4], prev[4], restarted)
        gaps.append(cur[1] - prev[1])
    host_read = rows[-1][6] - rows[0][6]
    peers = Counter(r[2] for r in rows)
    print(f"  rows {len(rows)}  reboots {len(reboots)}  received {received}  dropped {dropped} "
          f"({pct(dropped, received)})  host read {host_read}")
    print(f"  ~lost on USB {max(0, received - dropped - host_read)} "
          "(inferred from counters; +/-24 frames at each end)")
    print(f"  peer_count seen {dict(sorted(peers.items()))}")
    if gaps:
        print(f"  reply interval ms  {spread(gaps)}")
        interval = int(ctx.get("settings", {}).get("status_interval_ms", 5000))
        long_gaps = [(rows[i][1], g) for i, g in enumerate(gaps) if g > 3 * interval]
        for at, g in long_gaps[:5]:
            print(f"  no reply for {dur(g)} after {rel(at)}")
        if long_gaps:
            flag("WARN", "bridge",
                 f"{len(long_gaps)} stretch(es) with no bridge status reply for over 3x the "
                 f"{interval} ms poll; longest {dur(max(g for _, g in long_gaps))}",
                 "SELECT id, rx_at, rx_at - LAG(rx_at) OVER (ORDER BY id) gap FROM bridge_status "
                 "ORDER BY gap DESC LIMIT 10")
    ctx["bridge_reboots"] = reboots
    for at in reboots:
        print(f"  reboot near {rel(at)} ({utc(at)})")
    if reboots:
        flag("WARN", "bridge", f"bridge restarted {len(reboots)} time(s) (uptime fell)",
             "SELECT id, rx_at, uptime_ms FROM bridge_status ORDER BY id")
    if dropped:
        flag("WARN", "bridge", f"bridge dropped {dropped} frames it could not hand the host",
             "SELECT id, rx_at, dropped_tx FROM bridge_status ORDER BY id")
    # peer_count includes the broadcast peer the bridge registers at init, so a fleet of N reads N+1.
    fleet = len(ctx.get("nodes", {}))
    if peers and fleet and max(peers) > fleet + 1:
        print(f"  note: peer_count peaked at {max(peers)}, above {fleet} nodes + the broadcast peer")
    flat = 0
    longest_flat = (0, None)
    for prev, cur in zip(rows, rows[1:]):
        if cur[3] == prev[3] and cur[5] >= prev[5]:
            flat += cur[1] - prev[1]
            if flat > longest_flat[0]:
                longest_flat = (flat, cur[1] - flat)
        else:
            flat = 0
    if longest_flat[0] > 30_000:
        print(f"  bridge heard nothing over the air for {dur(longest_flat[0])} from "
              f"{rel(longest_flat[1])} (rx_count flat, bridge up)")


def host(db, ctx):
    section("host status")
    if not db.has("host_status", "at"):
        absent("host_status table")
        return
    cols = db.cols("host_status")
    order = [c for c in (
        "at", "frames", "duplicate_batches", "garbled", "undecodable", "incompatible",
        "foreign_fleet", "foreign_admin", "admin_windows_missed", "lag_peak_us", "store_written",
        "store_dropped", "store_queue_peak", "store_commit_peak_us", "throttled", "soc_temp_mc",
        "battery_mv", "battery_ma") if c in cols]
    rows = [dict(zip(order, r)) for r in db.q(f"SELECT {', '.join(order)} FROM host_status "
                                               "ORDER BY id")]
    if not rows:
        print("  no rows")
        return
    first, last = rows[0], rows[-1]
    deltas = {}
    for c in ("frames", "duplicate_batches", "garbled", "undecodable", "incompatible",
              "foreign_fleet", "foreign_admin", "admin_windows_missed", "store_written",
              "store_dropped"):
        if c in first:
            deltas[c] = (last[c] or 0) - (first[c] or 0)
    print("  " + "  ".join(f"{k} {v}" for k, v in deltas.items()))
    for c in ("lag_peak_us", "store_queue_peak", "store_commit_peak_us"):
        if c in first:
            print(f"  {c:<22}{spread([r[c] for r in rows if r[c] is not None])}")
    if "lag_peak_us" in first:
        behind = sum(1 for r in rows if (r["lag_peak_us"] or 0) >= 100_000)
        if behind:
            flag("WARN" if behind > 5 else "INFO", "host",
                 f"host fell >=100 ms behind the air in {behind} of {len(rows)} samples "
                 "(too stale to answer a heartbeat's admin window)",
                 "SELECT at, lag_peak_us FROM host_status WHERE lag_peak_us >= 100000")
    for c, sev in (("store_dropped", "WARN"), ("garbled", "WARN"), ("undecodable", "WARN"),
                   ("incompatible", "WARN"), ("foreign_fleet", "INFO"), ("foreign_admin", "INFO"),
                   ("admin_windows_missed", "WARN")):
        if deltas.get(c):
            flag(sev, "host", f"host_status {c} rose by {deltas[c]} over the capture",
                 f"SELECT at, {c} FROM host_status ORDER BY id")
    gaps = [(a["at"], b["at"] - a["at"]) for a, b in zip(rows, rows[1:])]
    long_gaps = [(at, g) for at, g in gaps if g > 15_000]
    for at, g in long_gaps[:5]:
        print(f"  no host sample for {dur(g)} after {rel(at)}")
    if long_gaps:
        flag("WARN", "host",
             f"{len(long_gaps)} gap(s) over 15 s between 5 s host samples: the host stalled, "
             "slept or lost power",
             "SELECT at, at - LAG(at) OVER (ORDER BY id) gap FROM host_status ORDER BY gap DESC")
    if "throttled" in first:
        words = [r["throttled"] for r in rows if r["throttled"] is not None]
        if words:
            under = sum(1 for w in words if w & 1)
            thr = sum(1 for w in words if w & 4)
            print(f"  pi throttle: under-voltage now in {under}, throttled now in {thr} of "
                  f"{len(words)}; since-boot bits {words[-1] >> 16:#06b}")
            if under:
                flag("WARN", "power", f"Pi reported under-voltage in {under} samples",
                     "SELECT at, throttled FROM host_status WHERE throttled & 1")
    if "soc_temp_mc" in first:
        temps = [r["soc_temp_mc"] for r in rows if r["soc_temp_mc"] is not None]
        if temps:
            print(f"  soc temp C  max {max(temps) / 1000:.1f}  p50 "
                  f"{statistics.median(temps) / 1000:.1f}")
            if max(temps) > 80_000:
                flag("WARN", "power", f"SoC reached {max(temps) / 1000:.1f} C")
    if "battery_mv" in first:
        mv = [r["battery_mv"] for r in rows if r["battery_mv"] is not None]
        if mv:
            print(f"  battery V  first {mv[0] / 1000:.2f}  last {mv[-1] / 1000:.2f}  "
                  f"min {min(mv) / 1000:.2f}")
        ma = [r.get("battery_ma") for r in rows if r.get("battery_ma") is not None]
        if ma:
            discharging = sum(1 for a in ma if a < 0)
            print(f"  battery mA  {spread(ma)}  discharging in {discharging} of {len(ma)} samples")
            if discharging > 0.9 * len(ma) and mv and mv[0] - mv[-1] > 200:
                flag("WARN", "power",
                     f"host ran on its battery the whole capture: {mv[0] / 1000:.2f} V -> "
                     f"{mv[-1] / 1000:.2f} V, current negative in {pct(discharging, len(ma))} "
                     "of samples (not charging)",
                     "SELECT at, battery_mv, battery_ma FROM host_status ORDER BY id")


def observations(db, ctx):
    section("observations")
    if not db.has("observation", "node_mac", "rx_at"):
        absent("observation table")
        return
    total = db.one("SELECT COUNT(*) FROM observation")
    print(f"  rows {total}")
    if not total:
        flag("WARN", "sightings", "no observations at all")
        return
    for k, n in db.q("SELECT kind, COUNT(*) FROM observation GROUP BY 1"):
        print(f"  kind {k}: {n}")
    kinds = dict(db.q("SELECT kind, COUNT(*) FROM observation GROUP BY 1"))
    unknown = {k: n for k, n in kinds.items() if k not in ("wifi", "ble")}
    if unknown:
        flag("WARN", "sightings", f"observations of unknown kind {unknown}: the export skips them",
             "SELECT kind, COUNT(*) FROM observation GROUP BY 1")
    if ctx.get("ble_nodes") and not kinds.get("ble"):
        flag("WARN", "sightings",
             "a node held the Bluetooth job but no ble observation was stored",
             "SELECT * FROM assignment WHERE ble=1")
    sec = db.q("SELECT security, COUNT(*) FROM observation GROUP BY 1 ORDER BY 2 DESC")
    print("  security " + "  ".join(f"{s} {n}" for s, n in sec))
    und = dict(sec).get("[UNDEFINED]", 0)
    if und:
        flag("INFO", "decode", f"{und} observations with security [UNDEFINED] ({pct(und, total)})",
             "SELECT channel, kind, COUNT(*) FROM observation WHERE security='[UNDEFINED]' "
             "GROUP BY 1,2")

    print("  by node: rows / distinct bssid / channels")
    per_node = db.q("SELECT node_mac, COUNT(*), COUNT(DISTINCT bssid), MIN(rx_at), MAX(rx_at) "
                    "FROM observation GROUP BY 1 ORDER BY 1")
    for mac, n, distinct, lo, hi in per_node:
        if ctx["only"] and short(mac) != ctx["only"]:
            continue
        chans = [c for (c,) in db.q("SELECT DISTINCT channel FROM observation WHERE node_mac=? "
                                    "ORDER BY 1", (mac,))]
        print(f"    {short(mac)}  {n}  {distinct}  {rel(lo)}..{rel(hi)}  ch {chans}")
        assigned = ctx.get("assigned", {}).get(mac)
        if assigned:
            off5 = [c for c in chans if c > 14 and c not in assigned]
            if off5:
                cnt = db.one(
                    f"SELECT COUNT(*) FROM observation WHERE node_mac=? AND channel IN "
                    f"({','.join('?' * len(off5))})", (mac, *off5))
                print(f"           5 GHz rows on channels never assigned to it: {off5} ({cnt})")
    first_obs = db.one("SELECT MIN(rx_at) FROM observation")
    if ctx.get("start") and first_obs and first_obs - ctx["start"] > 120_000:
        first_hb = min((h[2] for hs in ctx.get("hb", {}).values() for h in hs), default=None)
        print(f"  first sighting {rel(first_obs)}; first heartbeat {rel(first_hb)}")
        flag("WARN", "startup",
             f"nothing was stored for {dur(first_obs - ctx['start'])} after the capture began "
             f"(first heartbeat {rel(first_hb)})",
             "SELECT MIN(rx_at) - (SELECT started_at FROM capture) FROM heartbeat")
    heartbeating = set(ctx.get("hb", {}))
    for mac in heartbeating - {r[0] for r in per_node}:
        if mac not in ctx.get("ble_nodes", set()):
            flag("WARN", "sightings",
                 f"{short(mac)} heartbeated but stored no observations")
    pool_seen = {c for (c,) in db.q("SELECT DISTINCT channel FROM observation WHERE kind='wifi'")}
    assigned_all = set().union(*ctx.get("assigned", {}).values()) if ctx.get("assigned") else set()
    never = sorted(assigned_all - pool_seen)
    if never:
        print(f"  assigned channels with no sighting at all: {never}")

    # Sighting silence while the node still heartbeats.
    silent = db.q(
        "SELECT node_mac, rx_at, gap FROM (SELECT node_mac, rx_at, rx_at - LAG(rx_at) OVER "
        "(PARTITION BY node_mac ORDER BY id) gap FROM observation) WHERE gap > 120000 "
        "ORDER BY gap DESC LIMIT 20")
    hb = ctx.get("hb", {})
    hits = []
    for mac, rx, gap in silent:
        beats = sum(1 for h in hb.get(mac, []) if rx - gap < h[2] < rx)
        if beats > 3:
            hits.append((mac, rx, gap, beats))
    for mac, rx, gap, beats in hits[:8]:
        print(f"  {short(mac)} stored no sighting for {dur(gap)} before {rel(rx)} while sending "
              f"{beats} heartbeats")
    if hits:
        flag("WARN", "sightings",
             f"{len(hits)} stretch(es) over 2 min where a node heartbeated but stored no sighting",
             "SELECT node_mac, rx_at, gap FROM (SELECT node_mac, rx_at, rx_at - LAG(rx_at) OVER "
             "(PARTITION BY node_mac ORDER BY id) gap FROM observation) WHERE gap > 120000")

    odd = db.q(
        "SELECT SUM(bssid = zeroblob(6)), SUM(hex(bssid) = 'FFFFFFFFFFFF'), "
        "SUM(length(bssid) != 6), SUM(rssi >= 0 OR rssi < -110), SUM(ssid IS NULL), "
        "SUM(length(ssid) = 0) FROM observation WHERE kind='wifi'")[0]
    labels = ["zero bssid", "broadcast bssid", "bssid not 6 bytes", "rssi outside -110..-1",
              "ssid NULL", "ssid empty (hidden)"]
    print("  " + "  ".join(f"{l} {v or 0}" for l, v in zip(labels, odd)))
    for l, v in zip(labels[:4], odd[:4]):
        if v:
            flag("INFO" if l == "zero bssid" else "WARN", "decode",
                 f"{v} wifi observations with {l}"
                 + (" (export drops them; see whether several nodes heard them at once)"
                    if l == "zero bssid" else ""),
                 "SELECT * FROM observation WHERE kind='wifi' LIMIT 20")
    mcast = db.one("SELECT COUNT(*) FROM observation WHERE kind='wifi' AND "
                   "instr('13579BDF', substr(hex(bssid), 2, 1)) > 0")
    if mcast:
        print(f"  multicast-bit bssid {mcast}")
        flag("WARN", "decode", f"{mcast} wifi observations whose bssid has the multicast bit set",
             "SELECT hex(bssid), COUNT(*) FROM observation WHERE kind='wifi' AND "
             "instr('13579BDF', substr(hex(bssid), 2, 1)) > 0 GROUP BY 1")
    scan = ctx.get("scan")
    if scan:
        off = db.q(f"SELECT channel, COUNT(*) FROM observation WHERE kind='wifi' AND channel NOT IN "
                   f"({','.join(map(str, scan))}) GROUP BY 1")
        if off:
            print("  channels outside the scan table: "
                  + "  ".join(f"{c}:{n}" for c, n in off))
            flag("WARN", "decode",
                 f"{sum(n for _, n in off)} wifi observations on channels outside the scan table "
                 f"{[c for c, _ in off]}",
                 "SELECT channel, node_mac, COUNT(*) FROM observation WHERE channel NOT IN (...) "
                 "GROUP BY 1, 2")

    rev = db.one("SELECT COUNT(*) FROM (SELECT rx_at - LAG(rx_at) OVER (ORDER BY id) d "
                 "FROM observation) WHERE d < -2000")
    if rev:
        flag("WARN", "clock", f"observation rx_at stepped back by >2 s {rev} time(s)",
             "SELECT id, rx_at, rx_at - LAG(rx_at) OVER (ORDER BY id) d FROM observation "
             "ORDER BY d LIMIT 10")

    # Fleet-wide throughput over time, in 5-minute buckets.
    start = ctx.get("start") or db.one("SELECT MIN(rx_at) FROM observation")
    buckets = dict(db.q("SELECT (rx_at - ?) / 300000, COUNT(*) FROM observation GROUP BY 1",
                        (start,)))
    if buckets:
        lo, hi = min(buckets), max(buckets)
        series = [buckets.get(b, 0) for b in range(lo, hi + 1)]
        print(f"  rows per 5 min  {spread(series)}")
        empty = [b for b in range(lo, hi + 1) if not buckets.get(b)]
        if empty:
            print(f"  empty 5-min windows at +{', +'.join(str(b * 5) + 'm' for b in empty[:10])}")


def gps(db, ctx):
    section("positions")
    if not db.has("observation", "pos_source"):
        absent("observation.pos_source")
        return
    total = db.one("SELECT COUNT(*) FROM observation") or 0
    mix = dict(db.q("SELECT pos_source, COUNT(*) FROM observation GROUP BY 1"))
    print("  sources " + "  ".join(f"{k} {v} ({pct(v, total)})" for k, v in mix.items()))
    if mix.get("none", 0) > 0.05 * total:
        flag("WARN", "gps", f"{pct(mix['none'], total)} of observations have no position",
             "SELECT (rx_at - (SELECT started_at FROM capture))/60000 m, COUNT(*) FROM observation "
             "WHERE pos_source='none' GROUP BY 1")
    if mix.get("static"):
        flag("WARN", "gps", "static positions present: test data, never uploadable")
    if not db.has("observation", "lat", "lon", "pos_at"):
        return
    ages = [a for (a,) in db.q("SELECT rx_at - pos_at FROM observation WHERE pos_at IS NOT NULL")]
    if ages:
        print(f"  fix age at sighting ms  {spread(ages)}")
        limit = int(ctx.get("settings", {}).get("gps_max_age_ms", 5000))
        stale = sum(1 for a in ages if a > limit)
        if stale:
            flag("WARN", "gps", f"{stale} observations carry a fix older than gps_max_age_ms "
                 f"({limit})", "SELECT * FROM observation WHERE rx_at - pos_at > " + str(limit))
        neg = sum(1 for a in ages if a < -1000)
        if neg:
            flag("INFO", "clock", f"{neg} observations whose fix is >1 s newer than the sighting "
                 "(pos_at > rx_at): GPS time and host clock disagree")
    acc = [a for (a,) in db.q("SELECT accuracy FROM observation WHERE accuracy IS NOT NULL")]
    if acc:
        print(f"  accuracy m  p50 {pctile(acc, 50):.1f}  p95 {pctile(acc, 95):.1f}  "
              f"max {max(acc):.1f}")
    fixes = db.q("SELECT pos_at, lat, lon FROM observation WHERE pos_at IS NOT NULL AND lat IS NOT "
                 "NULL GROUP BY pos_at ORDER BY pos_at")
    jumps = []
    speeds = []
    for (t0, a0, o0), (t1, a1, o1) in zip(fixes, fixes[1:]):
        dt = (t1 - t0) / 1000
        if dt <= 0:
            continue
        p0, p1 = math.radians(a0), math.radians(a1)
        dl = math.radians(o1 - o0)
        h = (math.sin((p1 - p0) / 2) ** 2
             + math.cos(p0) * math.cos(p1) * math.sin(dl / 2) ** 2)
        d = 2 * 6_371_000 * math.asin(min(1, math.sqrt(h)))
        speeds.append(d / dt)
        if d / dt > 70 and d > 200:
            jumps.append((t1, d, dt))
    if speeds:
        print(f"  ~speed km/h between fixes  p50 {pctile(speeds, 50) * 3.6:.0f}  "
              f"p95 {pctile(speeds, 95) * 3.6:.0f}  (inferred from fix spacing)")
        gaps = [(t1 - t0) for (t0, *_), (t1, *_) in zip(fixes, fixes[1:])]
        long = [g for g in gaps if g > 30_000]
        if long:
            print(f"  {len(long)} gaps over 30 s between distinct fixes; longest {dur(max(long))}")
    for t, d, dt in jumps[:5]:
        print(f"  jump {d:.0f} m in {dt:.1f} s at {rel(t)}")
    if jumps:
        flag("INFO", "gps", f"{len(jumps)} position jump(s) faster than 250 km/h",
             "SELECT DISTINCT pos_at, lat, lon FROM observation ORDER BY pos_at")


def uploads(db, ctx):
    section("uploads")
    if not db.has("upload", "result"):
        absent("upload table")
        return
    rows = db.q("SELECT uploaded_at, job_id, rows, through_id, result FROM upload ORDER BY id")
    if not rows:
        print("  none")
        return
    total = db.one("SELECT MAX(id) FROM observation")
    for at, job, n, through, result in rows:
        print(f"  job {job}  {utc(at)} ({rel(at)})  {n} rows  through observation {through} "
              f"of {total}  result {result}")
        if result in (None, "unfollowed", "failed"):
            flag("WARN" if result == "failed" else "INFO", "upload",
                 f"upload job {job} result is {result!r}: whether the site imported it is not "
                 "recorded (check the job on the site; never re-upload without asking)",
                 "SELECT * FROM upload")
    junk = db.one(
        "SELECT COUNT(DISTINCT bssid) FROM observation WHERE kind='wifi' AND id <= ? AND "
        "(bssid = zeroblob(6) OR instr('13579BDF', substr(hex(bssid), 2, 1)) > 0)",
        (rows[-1][3],))
    if junk:
        print(f"  {junk} zero/group BSSIDs lie inside the uploaded range; the current export "
              "drops them, but compare the uploaded row count with `wartui export` to see whether "
              "the build that uploaded did")


def raw_frames(db, ctx):
    section("raw frames")
    if not db.has("raw_frame", "bytes"):
        absent("raw_frame table")
        return
    n = db.one("SELECT COUNT(*) FROM raw_frame")
    print(f"  rows {n}")
    if not n:
        return
    by_src = db.q("SELECT src, COUNT(*) FROM raw_frame GROUP BY 1 ORDER BY 2 DESC LIMIT 30")
    known = set(ctx.get("nodes", {}))
    for src, c in by_src:
        print(f"    {short(src)}  {c}{'' if src in known else '  (not in node table)'}")
    heads = Counter()
    for (b,) in db.q("SELECT substr(bytes, 1, 6) FROM raw_frame"):
        if b[:4] == b"WTUI" and len(b) >= 6:
            heads[f"WTUI v{b[4]} type {b[5]}"] += 1
        else:
            heads["other"] += 1
    print("  headers " + "  ".join(f"{k}: {v}" for k, v in heads.most_common()))
    # Type 1 is a heartbeat. Equal counts mean any heartbeat loss happened before the bridge.
    raw_hb = dict(db.q("SELECT src, COUNT(*) FROM raw_frame "
                       "WHERE hex(substr(bytes, 1, 6)) = '575455490101' GROUP BY 1"))
    stored = {m: len(h) for m, h in ctx.get("hb", {}).items()}
    if raw_hb:
        print("  heartbeat frames raw/stored per node: " + "  ".join(
            f"{short(m)} {raw_hb.get(m, 0)}/{stored.get(m, 0)}" for m in sorted(stored)))


LOG_LINE = re.compile(
    r"^(?P<ts>\d{4}-\d\d-\d\dT[\d:.]+Z?)\s+(?P<level>TRACE|DEBUG|INFO|WARN|ERROR)\s+"
    r"(?P<target>[\w:]+):\s?(?P<msg>.*)$")
KEYWORDS = re.compile(r"reconnect|identify|stall|reset|reboot|disconnect|timed? ?out|panic|"
                      r"wedge|Ready|uptime|gps|nmea|dropped|error", re.I)


def template(msg):
    msg = re.sub(r"\b[0-9A-Fa-f]{2}(:[0-9A-Fa-f]{2}){1,5}\b", "<mac>", msg)
    msg = re.sub(r"/dev/\S+", "<dev>", msg)
    return re.sub(r"\d+(\.\d+)?", "#", msg)[:160]


def parse_ts(ts):
    try:
        return int(datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() * 1000)
    except ValueError:
        return None


def log(path, ctx):
    section(f"log {path}")
    try:
        lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
    except OSError as e:
        print(f"  cannot read: {e}")
        return
    levels = Counter()
    targets = Counter()
    templates = Counter()
    first_at = {}
    unparsed = 0
    events = []
    start, end = ctx.get("start"), ctx.get("end")
    parsed = []
    for line in lines:
        m = LOG_LINE.match(line)
        if not m:
            unparsed += 1
            continue
        parsed.append((parse_ts(m["ts"]), m["level"], m["target"], m["msg"]))
    # A log file is appended to, so it may hold other runs. The capture is created as the process
    # starts, so this run's "wartui starting" line falls just before capture.started_at; keep from
    # it to the next run's start.
    run_starts = sorted(p[0] for p in parsed if "wartui starting" in p[3] and p[0])
    lo, hi = None, None
    if start and run_starts:
        mine = [t for t in run_starts if start - 300_000 <= t <= start + 5_000]
        if mine:
            lo = mine[-1]
            later = [t for t in run_starts if t > lo]
            hi = later[0] if later else None
        else:
            lo, hi = start - 60_000, (end or start) + 60_000
    elif start:
        lo, hi = start - 60_000, (end + 60_000) if end else None
    outside = 0
    for at, lv, tgt, msg in parsed:
        if lo is not None and at is not None and (at < lo or (hi is not None and at >= hi)):
            outside += 1
            continue
        levels[lv] += 1
        targets[tgt] += 1
        if lv in ("WARN", "ERROR") or KEYWORDS.search(msg):
            key = (lv, tgt, template(msg))
            templates[key] += 1
            first_at.setdefault(key, at)
            events.append((at, lv, tgt, msg))
    print(f"  lines {len(lines)}  unparsed {unparsed}  this run's levels {dict(levels)}")
    print(f"  targets {dict(targets.most_common(8))}")
    if len(run_starts) > 1:
        print(f"  {len(run_starts)} 'wartui starting' lines: the file spans several runs "
              f"({', '.join(utc(s) for s in run_starts)})")
    mine = [p[0] for p in parsed if p[0] and lo is not None and lo <= p[0] < (hi or 2**62)]
    if mine:
        print(f"  this run's lines: {rel(min(mine))} .. {rel(max(mine))}")
        if end and end - max(mine) > 600_000:
            print(f"  the log goes quiet {dur(end - max(mine))} before the capture ends "
                  "(at INFO, a healthy link logs little; nothing here covers the rest)")
    if outside:
        print(f"  {outside} lines from other runs or outside the capture are left out")
    if lo is not None and not any(lo <= (p[0] or 0) < (hi or 2**62) for p in parsed):
        print("  no line in the log belongs to this capture's run")
        flag("INFO", "log", "the log holds no lines from this capture's run: it is another run's")
    print("  notable messages (count, level, target, template, first seen):")
    for (lv, tgt, tpl), c in templates.most_common(25):
        print(f"    {c:>5}  {lv:<5} {tgt}  {tpl}   first {rel(first_at[(lv, tgt, tpl)])}")
    errors = sum(c for (lv, *_), c in templates.items() if lv == "ERROR")
    warns = sum(c for (lv, *_), c in templates.items() if lv == "WARN")
    if errors:
        flag("WARN", "log", f"{errors} ERROR lines in the log")
    if warns:
        flag("INFO", "log", f"{warns} WARN lines in the log")
    moments = [(at, "bridge reboot") for at in ctx.get("bridge_reboots", [])]
    moments += [(r[0], f"{short(r[1])} restart") for r in ctx.get("restarts", [])[:6]]
    for at, what in moments:
        near = [e for e in events if e[0] and abs(e[0] - at) <= 60_000]
        print(f"  within 60 s of {what} at {rel(at)}: {len(near)} notable lines")
        for e in near[:8]:
            print(f"    {rel(e[0])}  {e[1]}  {e[2]}: {e[3][:140]}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("db")
    ap.add_argument("--log", help="the --log-file from the same run")
    ap.add_argument("--node", help="limit per-node detail to one node, by last two octets")
    args = ap.parse_args()
    db = Db(args.db)
    ctx = {"only": args.node.upper() if args.node else None}
    print(f"survey of {args.db} (read-only)")
    print(f"tables: {', '.join(sorted(db.tables))}")
    provenance(db, ctx)
    nodes(db, ctx)
    # assignments first, quietly, so heartbeats can tell the Bluetooth node from the sweepers
    if db.has("assignment", "ble"):
        ctx["ble_nodes"] = {m for (m,) in db.q("SELECT DISTINCT node_mac FROM assignment "
                                              "WHERE ble=1")}
    heartbeats(db, ctx)
    assignments(db, ctx)
    batch_gaps(db, ctx)
    bridge(db, ctx)
    host(db, ctx)
    observations(db, ctx)
    gps(db, ctx)
    raw_frames(db, ctx)
    uploads(db, ctx)
    if args.log:
        log(args.log, ctx)

    section("FLAGS")
    order = {"ERROR": 0, "WARN": 1, "INFO": 2}
    for sev, area, text, sql in sorted(FLAGS, key=lambda f: order[f[0]]):
        print(f"  [{sev}] {area}: {text}")
        if sql:
            print(f"         {sql}")
    if not FLAGS:
        print("  none")


if __name__ == "__main__":
    try:
        main()
    except sqlite3.Error as e:
        sys.exit(f"sqlite error: {e}")
