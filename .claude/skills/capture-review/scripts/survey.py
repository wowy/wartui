#!/usr/bin/env python3
"""Read-only survey of a wartui capture database, and optionally its --log-file.

Prints facts, section by section, then a FLAGS list. Each flag carries the SQL that reproduces
it. It interprets nothing beyond thresholds: the reviewer reads the source to say why.

The file is opened read-only (`mode=ro`), so a capture that is still being written is safe to
survey. It reads the tables and columns this build writes (`SCHEMA` in
crates/wartui-core/src/store.rs), and nothing else.

Nodes and slots print as the operator reads them: a node by its latest heartbeat's slot label
(`N7`), or by its last two octets with no slot or no carrier, and a slot by its label. Each flag's
SQL queries the raw rows, so it stays on bus names (`A1`) and MAC hex.

Usage: python3 -I survey.py CAPTURE.db [--log wartui.log] [--node N7]
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

# Reset causes that are the operator's own doing; every other stored cause is a fault.
# Mirrors `ResetCause::is_fault` in crates/wartui-proto/src/reset.rs.
NOT_FAULTS = ("power_on", "external")

# Mirrors store::SCHEMA_VERSION in crates/wartui-core/src/store.rs. The survey reads this shape
# only, as the store does.
SCHEMA_VERSION = 3

# Mirrors plan::MAX_NODES in crates/wartui-proto/src/plan/mod.rs.
MAX_NODES = 14

# Mirrors EngineConfig::topology_timeout in crates/wartui-core/src/engine/config.rs.
TOPOLOGY_TIMEOUT_MS = 20000

# Mirrors bus::PER_BUS and bus::MAX_SLOTS in crates/wartui-proto/src/bus/slot.rs.
PER_BUS = 7
MAX_SLOTS = 14

# Mirrors NodeBoard::per_bus in crates/wartui-proto/src/bus/node_board.rs. Any other value of
# capture.node_board has no labels.
BOARD_PER_BUS = {"carrier-6": 3, "carrier-10": 5, "carrier-14": 7}

# Mirrors EngineConfig::status_interval in crates/wartui-core/src/engine/config.rs.
STATUS_INTERVAL_MS = 5000

# Mirrors plan::SCAN_CHANNELS in crates/wartui-proto/src/plan/channels.rs.
SCAN_CHANNELS = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
    36, 40, 44, 48,
    52, 56, 60, 64,
    100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
    149, 153, 157, 161, 165, 169, 173, 177,
]


def is_fault(cause):
    return cause is not None and cause not in NOT_FAULTS


def flag(sev, area, text, sql=""):
    FLAGS.append((sev, area, text, sql.strip()))


# ---------------------------------------------------------------------------- helpers


def short(mac):
    """A MAC by its last two octets, as the docs name bridges and nodes with no slot."""
    if mac is None:
        return "?"
    if isinstance(mac, (bytes, bytearray)) and len(mac) == 6:
        return f"{mac[4]:02X}:{mac[5]:02X}"
    return str(mac)


def full(mac):
    if isinstance(mac, (bytes, bytearray)):
        return ":".join(f"{b:02X}" for b in mac)
    return str(mac)


def slot_name(k):
    """The bus name of the slot whose byte is `k`, as `heartbeat.slot` stores it."""
    return f"{'AB'[k // PER_BUS]}{k % PER_BUS + 1}"


BOARD = None  # capture.node_board, or None while it is NULL.
LATEST_SLOT = {}  # mac -> the bus name its latest heartbeat came from, with a carrier


def label(slot):
    """A bus name as the carrier's silk names it. Mirrors `NodeBoard::label`, so a slot the board
    does not drive stays as stored, as does every slot with no carrier or an unknown board."""
    per_bus = BOARD_PER_BUS.get(BOARD)
    if per_bus is None or not re.fullmatch("[AB][1-7]", str(slot)):
        return slot
    bus, index = "AB".index(slot[0]), int(slot[1]) - 1
    return f"N{bus * per_bus + index + 1}" if index < per_bus else slot


def load_slots(db):
    """Fill LATEST_SLOT. Heartbeat slots mean nothing to the operator with no carrier."""
    if BOARD not in BOARD_PER_BUS:
        return
    rows = db.q("SELECT node_mac, slot, MAX(id) FROM heartbeat GROUP BY node_mac")
    for mac, slot, _ in rows:
        if re.fullmatch("[AB][1-7]", str(slot)):
            LATEST_SLOT[mac] = slot


def name(mac):
    """What the operator calls a node: its slot's label, or its last two octets with no slot.
    Nodes whose latest heartbeats name one slot each add their octets, as `wartui analyze` does.
    """
    slot = LATEST_SLOT.get(mac)
    if slot is None:
        return short(mac)
    if sum(s == slot for s in LATEST_SLOT.values()) > 1:
        return f"{label(slot)} {short(mac)}"
    return label(slot)


def named(mac):
    """`name(mac)` tied to its address: `N7 (57:84)`."""
    node = name(mac)
    return node if short(mac) in node else f"{node} ({short(mac)})"


def node_order(mac):
    """Sorts as the carrier reads: by slot, then nodes with no slot by address. Bus names sort
    as their labels do."""
    slot = LATEST_SLOT.get(mac)
    return (slot is None, slot or "", mac)


def wanted(ctx, mac):
    """Whether --node leaves this node's detail in: no --node, its octets or its slot's label."""
    only = ctx["only"]
    if not only or only == short(mac):
        return True
    slot = LATEST_SLOT.get(mac)
    return slot is not None and only == label(slot)


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

    def q(self, sql, args=()):
        return self.conn.execute(sql, args).fetchall()

    def one(self, sql, args=()):
        row = self.conn.execute(sql, args).fetchone()
        return row[0] if row else None


def since_boot_delta(cur, prev, rebooted):
    """The increase of a since-boot counter, as the engine's advance_since_boot takes it."""
    if prev is None:
        return 0
    if rebooted or cur < prev:
        return cur
    return cur - prev


# ---------------------------------------------------------------------------- sections


def provenance(db, ctx):
    global START_MS, BOARD
    section("capture")
    cols = ("started_at", "ended_at", "bridge_mac", "bridge_fw", "node_board", "notes",
            "simulated")
    row = dict(zip(cols, db.q(f"SELECT {', '.join(cols)} FROM capture")[0]))
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
        f"  bridge    {full(row.get('bridge_mac'))}  fw {row.get('bridge_fw')}"
    )
    board = row.get("node_board")
    BOARD = board
    load_slots(db)
    per_bus = BOARD_PER_BUS.get(board)
    if per_bus:
        # The rows store bus names; the operator read the silk, and the survey prints it.
        print(f"  board     {board}  (slots below are labels; SQL and rows hold bus names: "
              f"A1 is N1, B1 is N{per_bus + 1})")
    else:
        print(f"  board     {board}")
    if board == "no carrier":
        flag("WARN", "capture",
             "node_board='no carrier': the P4 found no wartui-carrier at boot and drove no node",
             "SELECT node_board FROM capture")
    print(f"  simulated {row.get('simulated')}")
    if row.get("notes"):
        print(f"  notes     {row['notes']}")
    if row.get("simulated"):
        flag("WARN", "capture", "simulated=1: test data (--sim or --lat/--lon); never uploaded")

    kv = dict(db.q("SELECT k, v FROM kv"))
    ctx["kv"] = kv
    for k in sorted(kv):
        if not k.startswith("capture.settings."):
            print(f"  kv        {k} = {kv[k]}")

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
    # Settings-driven thresholds (gps_max_age_ms) read ctx["settings"]: the initial row overlaid
    # with every later change, so a value changed mid-run is judged by what was in force at the
    # end. The changes are listed above. Fixed engine timings come from the module constants.
    # The pool is the bridge's: `none` until its first `Status`, then each change it reported.
    # A capture with no initial row starts from its first change.
    pools = [(None, ctx["settings"]["pool"])] if "pool" in ctx.get("settings", {}) else []
    pools += [(at, ch["pool"][1]) for at, ch in ctx.get("settings_changes", []) if "pool" in ch]
    if pools:
        later = "".join(f" -> {new} at {rel(at)}" for at, new in pools[1:])
        print(f"  pool      {pools[0][1]}{later}")
    # The scan mode is the bridge's too: `none` until its first report, then each change.
    modes = [(None, ctx["settings"]["mode"])] if "mode" in ctx.get("settings", {}) else []
    modes += [(at, ch["mode"][1]) for at, ch in ctx.get("settings_changes", []) if "mode" in ch]
    if modes:
        later = "".join(f" -> {new} at {rel(at)}" for at, new in modes[1:])
        print(f"  mode      {modes[0][1]}{later}")
        if not any(m in ("wifi", "wifi+bluetooth") for _, m in modes):
            flag("WARN", "plan", "the scan mode never left stopped: no node was given a share",
                 "SELECT k, v FROM kv WHERE k LIKE 'capture.settings%'")
        elif ctx.get("end") and modes[-1][1] != "stopped":
            flag("INFO", "plan",
                 f"the capture ended with the scan mode {modes[-1][1]}: the bridge did not report "
                 "its scan stopped before the host quit, so the fleet was left scanning",
                 "SELECT k, v FROM kv WHERE k LIKE 'capture.settings%'")
    if "settings_final" in ctx:
        ctx["settings"] = {**ctx.get("settings", {}), **ctx["settings_final"]}
    raw_values = {c[1] for _, ch in ctx.get("settings_changes", []) for k, c in ch.items()
                  if k == "record_raw"}
    if raw_values:
        flag("INFO", "capture",
             "record_raw changed during the run, so raw_frame covers only part of it (see the "
             "settings changes above)")
    elif ctx.get("settings", {}).get("record_raw") == "false":
        flag(
            "INFO",
            "capture",
            "record_raw=false: raw_frame is empty, so frame-level questions cannot be answered "
            "from this capture",
        )


def nodes(db, ctx):
    section("nodes")
    rows = db.q("SELECT mac, first_seen, last_seen, capabilities FROM node ORDER BY mac")
    ctx["nodes"] = {r[0]: r for r in rows}
    for mac, first, last, caps in sorted(rows, key=lambda r: node_order(r[0])):
        if not wanted(ctx, mac):
            continue
        print(
            f"  {named(mac)}  {full(mac)}  {caps or 'no capabilities'}  "
            f"seen {rel(first)} .. {rel(last)}"
        )
        if caps is None:
            flag(
                "INFO",
                "nodes",
                f"{name(mac)} never reported capabilities: heard only by sighting, never "
                "assignable",
                f"SELECT * FROM node WHERE hex(mac)='{mac.hex().upper()}'",
            )
    ble_slot = ctx.get("settings", {}).get("ble_slot")
    if ble_slot and ble_slot != "none" and not ctx.get("ble_nodes"):
        flag(
            "INFO",
            "config",
            f"{label(ble_slot)} is the bridge's Bluetooth slot, but no share gave a node the "
            "Bluetooth job, so no node heartbeated from that slot and none scanned Bluetooth",
            "SELECT k, v FROM kv WHERE k LIKE 'capture.settings%'",
        )
    if len(rows) > MAX_NODES:
        print(f"  {len(rows)} nodes: more than a bridge's {MAX_NODES} slots, so some came and went")


# The fall in a node's free heap across one boot that reads as a leak rather than the Wi-Fi
# driver's own churn.
HEAP_FALL_BYTES = 2048


def heartbeats(db, ctx):
    section("heartbeats")
    # Row layout, read by index here and in assignments() and observations().
    rows = db.q("SELECT node_mac, id, rx_at, counter, epoch, slot, reset_cause, live, "
                "wifi_refused, ble_refused, vdd_mv, die_temp_c, heap_free FROM heartbeat "
                "ORDER BY node_mac, id")
    per = defaultdict(list)
    for r in rows:
        per[r[0]].append(r)
    ctx["hb"] = per
    ble_nodes = ctx.get("ble_nodes", set())
    total_reversals = 0
    for mac, hs in sorted(per.items(), key=lambda item: node_order(item[0])):
        reboots = replayed = 0
        wifi_ref = ble_ref = 0
        silences = []
        reversals = 0
        intervals = []
        slots = Counter()
        moves = []  # (rx_at, from slot, to slot)
        # The reset cause of each boot seen: the first beat's, then each restart's.
        boots = [hs[0][6]] if hs else []
        prev = None
        # The engine's `held_epoch`: the last live beat's epoch, None until one and after a restart.
        held = None
        # Where the node's last boot in the capture starts, for its heap.
        boot_start = 0
        for i, r in enumerate(hs):
            (_, _, rx, counter, epoch, slot, cause, live, wr, br, _, _, _) = r
            replayed += live == 0
            slots[slot] += 1
            if prev is not None:
                p_rx, p_counter, p_slot = prev[2], prev[3], prev[5]
                if slot != p_slot:
                    moves.append((rx, p_slot, slot))
                # As the engine reads a restart: the counter fell, or a live beat reports no
                # epoch after a live beat reported a real one.
                rebooted = ((counter - p_counter) % 2**32 >= 2**31
                            or (live != 0 and epoch == 0 and held not in (None, 0)))
                if rebooted:
                    reboots += 1
                    boot_start = i
                    boots.append(cause)
                    ctx.setdefault("restarts", []).append((rx, mac, p_counter, counter, p_rx,
                                                           cause))
                    held = None
                wifi_ref += since_boot_delta(wr, prev[8], rebooted)
                ble_ref += since_boot_delta(br, prev[9], rebooted)
                dt = rx - p_rx
                if dt < 0:
                    reversals += 1
                elif prev[7] != 0:  # an interval after a replayed beat spans time no host read
                    intervals.append(dt)
                if dt > TOPOLOGY_TIMEOUT_MS:
                    silences.append((p_rx, rx))
            if live != 0:
                held = epoch
            prev = r
        total_reversals += reversals
        if not wanted(ctx, mac):
            continue  # walked anyway: its restarts feed the grouping and the adoption check
        job = "ble" if mac in ble_nodes else "wifi"
        where = "  ".join(f"{label(s)} {n}" for s, n in slots.most_common())
        print(f"  {name(mac)} [{job}]  rows {len(hs)}  slot {where}  restarts {reboots}  "
              f"replayed {replayed}")
        if intervals:
            print(f"        interval ms  {spread(intervals)}")
            # An assigned node beats once a sweep, at most every 5 s, so its median interval is
            # its cadence. Twice that is a beat that did not arrive.
            usual = max(statistics.median(intervals), 5000)
            late = [i for i in intervals if i > 2 * usual]
            if late:
                print(f"        ~{len(late)} gaps over twice its median interval of "
                      f"{usual:.0f} ms (inferred from timing)")
        if wifi_ref or ble_ref:
            print(f"        buffer full wifi {wifi_ref}  ble {ble_ref}  "
                  "(pending-buffer pressure, not loss)")
        node_sql = f"hex(node_mac)='{mac.hex().upper()}'"
        heaps = [r[12] for r in hs]
        if heaps:
            print(f"        heap free    first {heaps[0]}  min {min(heaps)}  last {heaps[-1]}")
            # Over the last boot alone, since a restart refills the heap. The driver's own churn
            # moves it a little, so only a fall from the boot's first fifth to its last counts.
            boot = heaps[boot_start:]
            fifth = len(boot) // 5
            if fifth >= 4:
                fell = min(boot[:fifth]) - min(boot[-fifth:])
                if fell > HEAP_FALL_BYTES:
                    flag("WARN", "node",
                         f"{name(mac)} free heap fell {fell} bytes across its last boot "
                         f"({min(boot[:fifth])} -> {min(boot[-fifth:])} at the low points): "
                         "a leak in the node firmware?",
                         f"SELECT id, rx_at, counter, heap_free FROM heartbeat WHERE {node_sql} "
                         "ORDER BY id")
        for at, a, b in moves[:5]:
            print(f"        moved        {label(a)} -> {label(b)} at {rel(at)}")
        if moves:
            flag("INFO", "node",
                 f"{name(mac)} heartbeated from {len(slots)} slots "
                 f"({', '.join(str(label(s)) for s in slots)}), "
                 f"moving {len(moves)} time(s), first near {rel(moves[0][0])}",
                 f"SELECT id, rx_at, slot FROM (SELECT *, LAG(slot) OVER (ORDER BY id) AS ps "
                 f"FROM heartbeat WHERE {node_sql}) WHERE slot != ps ORDER BY id")
        epochs = Counter(r[4] for r in hs)
        print(f"        epochs held  {dict(sorted(epochs.items()))}")
        if any(c is not None for c in boots):
            # The first beat names a boot that may predate the capture; the rest are restarts
            # this capture saw.
            before, during = boots[0], boots[1:]
            print(f"        boots        before capture: {before}"
                  + (f"; in capture: {', '.join(str(c) for c in during)}" if during else ""))
            if is_fault(before):
                flag("INFO", "node",
                     f"{name(mac)} last booted before the capture started, for {before}",
                     f"SELECT id, rx_at, counter, reset_cause FROM heartbeat WHERE "
                     f"{node_sql} ORDER BY id LIMIT 1")
            faults = [c for c in during if is_fault(c)]
            if faults:
                not_faults = ", ".join(f"'{c}'" for c in NOT_FAULTS)
                flag("WARN", "node",
                     f"{name(mac)} restarted {len(faults)} time(s) in the capture for a cause "
                     f"other than power-on or an external reset: {', '.join(faults)}",
                     f"SELECT id, rx_at, counter, reset_cause FROM (SELECT *, LAG(counter) "
                     f"OVER (ORDER BY id) AS pc FROM heartbeat WHERE {node_sql}) WHERE counter < pc "
                     f"AND reset_cause NOT IN ({not_faults}) ORDER BY id")
        for a, b in silences[:5]:
            print(f"        silent       {rel(a)} -> {rel(b)}  ({dur(b - a)})")
        if silences:
            flag(
                "WARN",
                "node",
                f"{name(mac)} went {len(silences)} time(s) without a heartbeat for longer than "
                f"topology_timeout ({TOPOLOGY_TIMEOUT_MS} ms); "
                f"longest {dur(max(b - a for a, b in silences))}",
                f"SELECT id, rx_at, rx_at - LAG(rx_at) OVER (ORDER BY id) AS gap FROM heartbeat "
                f"WHERE {node_sql} ORDER BY gap DESC LIMIT 10",
            )
    restarts = sorted(ctx.get("restarts", []))
    groups = []
    for r in restarts:
        if groups and r[0] - groups[-1][-1][0] <= 30_000:
            groups[-1].append(r)
        else:
            groups.append([r])
    fleet = len(per)
    for g in groups:
        macs = [name(m) for m in sorted({r[1] for r in g}, key=node_order)]
        at = g[0][0]
        silent = max(r[0] - r[4] for r in g)
        detail = ", ".join(f"{named(r[1])} counter {r[2]}->{r[3]}"
                           + (f" reset {r[5]}" if r[5] is not None else "") for r in g)
        causes = sorted({f"{name(r[1])} {r[5]}" for r in g if r[5] is not None})
        print(f"  restart near {rel(at)}: {', '.join(macs)}  (longest silence {dur(silent)})"
              + (f"  reset {', '.join(causes)}" if causes else ""))
        who = (f"all {fleet} nodes restarted together" if len(macs) == fleet and fleet > 1
               else f"{', '.join(macs)} restarted")
        flag("WARN", "node",
             f"{who} near {rel(at)} ({utc(at)}); longest heartbeat silence {dur(silent)}"
             + ("; a shared cause (power, bridge, host) is more likely than node firmware"
                if len(macs) > 1 else ""),
             f"-- {detail}\nSELECT id, node_mac, rx_at, counter, epoch FROM heartbeat WHERE rx_at "
             f"BETWEEN {at - 60000} AND {at + 30000} ORDER BY id")
    end = ctx.get("end")
    lasts = [hs[-1][2] for hs in per.values() if hs]
    if end and len(lasts) > 1 and end - max(lasts) > 30_000 and max(lasts) - min(lasts) < 15_000:
        print(f"  every node fell silent within {dur(max(lasts) - min(lasts))} of each other, "
              f"{dur(end - max(lasts))} before the capture ended (fleet power off before quit?)")
    if total_reversals:
        flag("WARN", "clock",
             f"heartbeat rx_at stepped backwards {total_reversals} time(s) in arrival order: the "
             "host wall clock moved (NTP/GPS time set?)",
             "SELECT id, rx_at, rx_at - LAG(rx_at) OVER (ORDER BY id) d FROM heartbeat "
             "ORDER BY d LIMIT 10")


def share_node(db, mac, slot, at):
    """The node a share row is for: its `node_mac`, or with none, the first node to heartbeat from
    its slot at or after the row."""
    if mac is not None:
        return mac
    return db.one("SELECT node_mac FROM heartbeat WHERE slot=? AND rx_at>=? ORDER BY id LIMIT 1",
                  (slot, at))


def assignments(db, ctx):
    section("assignments")
    # One row per slot whose share changed between two of the P4's `Fleet` reports. The host
    # sends nothing and sees no delivery: a heartbeat carrying the row's epoch is adoption.
    rows = db.q("SELECT id, rx_at, slot, node_mac, epoch, channels, ble FROM assignment "
                "ORDER BY id")
    rows = [(i, at, slot, share_node(db, mac, slot, at), epoch, chans, ble)
            for i, at, slot, mac, epoch, chans, ble in rows]
    withdrawn = sum(r[4] == 0 for r in rows)
    unnamed = db.one("SELECT COUNT(*) FROM assignment WHERE node_mac IS NULL")
    print(f"  rows {len(rows)}  withdrawn {withdrawn}  with no node yet {unnamed}")
    ble = {r[3] for r in rows if r[6] and r[3] is not None}
    ctx["ble_nodes"] = ble
    if ble:
        print(f"  bluetooth job given to {', '.join(name(m) for m in sorted(ble, key=node_order))}")
    if ctx.get("start") and ctx.get("end") and rows:
        hours = (ctx["end"] - ctx["start"]) / 3_600_000
        if hours > 0:
            print(f"  {len(rows) / hours:.1f} assignment rows per hour")
    assigned = defaultdict(set)
    for _, at, slot, mac, epoch, bits, is_ble in rows:
        chans = [c for i, c in enumerate(SCAN_CHANNELS) if bits >> i & 1]
        if mac is not None:
            assigned[mac] |= set(chans)
        who = name(mac) if mac is not None else label(slot)
        what = "withdrawn" if epoch == 0 else f"epoch {epoch}  {'BLE' if is_ble else ''}  " \
                                                f"{len(chans)} channels"
        print(f"  {who}  {what}  at {rel(at)}")
    ctx["assigned"] = assigned
    print("  final share per slot:")
    last = {}
    for r in rows:
        last[r[2]] = r
    for slot, r in sorted(last.items()):
        chans = [c for i, c in enumerate(SCAN_CHANNELS) if r[5] >> i & 1]
        who = name(r[3]) if r[3] is not None else label(slot)
        if r[4] == 0:
            print(f"    {who}  withdrawn")
        else:
            print(f"    {who}  " + " ".join(str(c) for c in chans) + ("  BLE" if r[6] else ""))

    # Adoption: while a share stands (from its row to the slot's next row, or the capture's end),
    # a live heartbeat from its node should carry its epoch. A node that kept heartbeating past
    # the grace and never did, never took the share.
    hb = ctx.get("hb", {})
    grace = 15_000
    never = defaultdict(list)
    for k, (_, at, slot, mac, epoch, _, _) in enumerate(rows):
        if epoch == 0 or mac is None:
            continue
        until = next((r[1] for r in rows[k + 1:] if r[2] == slot), None)
        restarted_at = [r[0] for r in ctx.get("restarts", []) if r[1] == mac]
        live = [h for h in hb.get(mac, [])
                if h[7] != 0 and h[2] >= at and (until is None or h[2] < until)
                and not any(0 <= h[2] - t <= 30_000 for t in restarted_at)]
        if any(h[4] == epoch for h in live):
            continue
        if any(h[2] >= at + grace for h in live):
            never[mac].append((at, epoch))
    for mac, shares in sorted(never.items(), key=lambda item: node_order(item[0])):
        at, epoch = shares[0]
        flag("WARN", "plan",
             f"{name(mac)}: {len(shares)} share(s) never adopted: its live heartbeats >15 s "
             f"after the row never carried the epoch (first epoch {epoch} at {rel(at)})",
             f"SELECT id, rx_at, slot, epoch FROM heartbeat WHERE hex(node_mac)="
             f"'{mac.hex().upper()}' AND rx_at >= {at} ORDER BY id LIMIT 20")


def batch_gaps(db, ctx):
    section("batch gaps")
    rows = db.q("SELECT node_mac, rx_at, lost FROM batch_gap ORDER BY id")
    if not rows:
        print("  none recorded")
        return
    per = defaultdict(list)
    for mac, rx, lost in rows:
        per[mac].append((rx, lost))
    for mac, gaps in sorted(per.items(), key=lambda item: node_order(item[0])):
        if not wanted(ctx, mac):
            continue
        total = sum(lost for _, lost in gaps)
        buckets = Counter((rx - (ctx.get("start") or 0)) // 300_000 for rx, _ in gaps)
        worst = buckets.most_common(3)
        print(
            f"  {name(mac)}  gaps {len(gaps)}  lost {total}  largest {max(l for _, l in gaps)}  "
            f"busiest 5-min windows "
            + ", ".join(f"+{b * 5}m:{n}" for b, n in worst)
        )
    total = sum(r[2] for r in rows)
    flag("INFO" if total < 50 else "WARN", "loss",
         f"{total} sighting batches lost between node and host across {len(per)} node(s)",
         "SELECT hex(node_mac), COUNT(*), SUM(lost) FROM batch_gap GROUP BY 1")


def bridge(db, ctx):
    section("bridge status")
    rows = db.q(
        "SELECT id, rx_at, rx_oversize, rx_count, dropped_tx, uptime_ms, host_frames "
        "FROM bridge_status ORDER BY id"
    )
    if not rows:
        print("  no status replies")
        flag("WARN", "bridge", "no bridge_status rows: the bridge never answered a status poll")
        return
    reboots = []
    received = dropped = oversize = 0
    gaps = []
    # Each status row's count of restarts so far: two rows a restart apart differ.
    lives = {rows[0][0]: 0}
    for prev, cur in zip(rows, rows[1:]):
        lives[cur[0]] = lives[prev[0]] + (cur[5] < prev[5])
    queues = bridge_queue_drops(db, lives)
    for prev, cur in zip(rows, rows[1:]):
        restarted = cur[5] < prev[5]
        if restarted:
            reboots.append(cur[1])
        received += since_boot_delta(cur[3], prev[3], restarted)
        dropped += since_boot_delta(cur[4], prev[4], restarted)
        oversize += since_boot_delta(cur[2], prev[2], restarted)
        gaps.append(cur[1] - prev[1])
    # dropped_tx sums queues on both chips, so neither restart rule fits it: as analyze does, take
    # the total from the queues when rows carry them.
    in_dropped_tx = [q[2] for name, q in queues.items() if name in DROPPED_TX_QUEUES]
    if in_dropped_tx:
        dropped = sum(in_dropped_tx)
    host_read = rows[-1][6] - rows[0][6]
    print(f"  rows {len(rows)}  reboots {len(reboots)}  received {received}  dropped {dropped} "
          f"({pct(dropped, received)})  host read {host_read}")
    print(f"  ~lost on USB {max(0, received - dropped - host_read)} "
          "(inferred from counters; +/-24 frames at each end)")
    if oversize:
        print(f"  oversize {oversize} (node records over 1024 bytes; not in received)")
        flag("WARN", "bridge",
             f"the P4 dropped {oversize} node record(s) over 1024 bytes, which no node of this "
             "build sends",
             "SELECT id, rx_at, uptime_ms, rx_oversize FROM bridge_status ORDER BY id")
    if gaps:
        print(f"  reply interval ms  {spread(gaps)}")
        long_gaps = [(rows[i][1], g) for i, g in enumerate(gaps) if g > 3 * STATUS_INTERVAL_MS]
        for at, g in long_gaps[:5]:
            print(f"  no reply for {dur(g)} after {rel(at)}")
        if long_gaps:
            flag("WARN", "bridge",
                 f"{len(long_gaps)} stretch(es) with no bridge status reply for over 3x the "
                 f"{STATUS_INTERVAL_MS} ms poll; longest {dur(max(g for _, g in long_gaps))}",
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
        print(f"  bridge forwarded no node record for {dur(longest_flat[0])} from "
              f"{rel(longest_flat[1])} (rx_count flat, bridge up)")
    bridge_queues(queues)
    bridge_radio_link(db, lives)
    bridge_host_uart(db)
    bridge_fan(db)
    bridge_slots(db)


# Mirror QueueId::on_p4 and QueueId::in_dropped_tx.
P4_QUEUES = {"radio-down", "host-priority", "host-bulk", "node-down"}
DROPPED_TX_QUEUES = {"radio-up", "host-priority", "host-bulk"}


def bridge_queue_drops(db, lives):
    """Each queue's capacity, peak and drops over the capture, rebased as analyze does: the uptime
    that dates a restart is the C6's, so the P4's queues rebase only when they fall."""
    rows = db.q("SELECT q.status_id, q.queue, q.capacity, q.high_water, q.dropped "
                "FROM bridge_queue q JOIN bridge_status s ON s.id = q.status_id "
                "ORDER BY s.id, q.rowid")
    queues = {}  # name -> [capacity, peak, dropped, prev dropped, prev life]
    for status_id, name, capacity, high_water, dropped in rows:
        life = lives.get(status_id, 0)
        q = queues.get(name)
        if q is None:
            queues[name] = [capacity, high_water, 0, dropped, life]
            continue
        q[0] = capacity
        q[1] = max(q[1], high_water)
        q[2] += since_boot_delta(dropped, q[3], name not in P4_QUEUES and life != q[4])
        q[3], q[4] = dropped, life
    return queues


def bridge_queues(queues):
    """Each queue's since-boot peak against its size, and what it dropped during the capture."""
    if not queues:
        print("  no queues reported")
        return
    for name, (capacity, peak, dropped, _, _) in queues.items():
        print(f"  queue {name}  peak {peak}/{capacity} bytes ({pct(peak, capacity)})  "
              f"dropped {dropped}")
        sql = ("SELECT s.id, s.rx_at, s.uptime_ms, q.capacity, q.high_water, q.dropped "
               "FROM bridge_queue q JOIN bridge_status s ON s.id = q.status_id "
               f"WHERE q.queue = '{name}' ORDER BY s.id")
        if dropped:
            flag("WARN", "bridge", f"bridge queue {name} dropped {dropped} during the capture", sql)
        if capacity and peak >= 0.9 * capacity:
            flag("WARN", "bridge",
                 f"bridge queue {name} peaked at {pct(peak, capacity)} of its {capacity} bytes "
                 "since the bridge booted: near full, so its WARTUI_* size is the lever", sql)


RADIO_LINK_COUNTERS = ("packets", "crc_fail", "bad_magic", "seq_gap", "uart_errors",
                "radio_crc_fail", "radio_bad_magic", "radio_seq_gap", "radio_uart_errors")


def bridge_radio_link(db, lives):
    """The P4–C6 UART link's counters over the capture, rebased across restarts.

    The uptime that dates a restart is the C6's. The `radio_*` counters restart with it; the P4's
    five run on, so they rebase only when they fall, as `RadioLinkLoss::advance` does. A P4 reboot
    whose counters regrow past the last row reads as no reboot, which is rare and only undercounts.
    """
    cols = ", ".join(f"radio_link_{c}" for c in RADIO_LINK_COUNTERS)
    rows = db.q(f"SELECT id, {cols} FROM bridge_status ORDER BY id")
    if not rows:
        return
    sums = dict.fromkeys(RADIO_LINK_COUNTERS, 0)
    for prev, cur in zip(rows, rows[1:]):
        restarted = lives.get(cur[0], 0) != lives.get(prev[0], 0)
        for i, c in enumerate(RADIO_LINK_COUNTERS, start=1):
            sums[c] += since_boot_delta(cur[i], prev[i], restarted and c.startswith("radio_"))
    print("  radio link " + "  ".join(f"{c} {sums[c]}" for c in RADIO_LINK_COUNTERS))
    sql = f"SELECT id, rx_at, uptime_ms, {cols} FROM bridge_status ORDER BY id"
    # A C6 restart breaks the seq the P4 hears exactly once, which is expected.
    restarts = lives.get(rows[-1][0], 0) - lives.get(rows[0][0], 0)
    unexplained = {**sums, "seq_gap": max(sums["seq_gap"] - restarts, 0)}
    errors = {c: unexplained[c] for c in RADIO_LINK_COUNTERS
              if c != "packets" and unexplained[c]}
    if errors:
        beyond = f" (seq_gap past {restarts} restart(s))" if restarts and "seq_gap" in errors else ""
        flag("WARN", "bridge",
             "UART link errors between P4 and C6: "
             + ", ".join(f"{c} {n}" for c, n in errors.items()) + beyond, sql)


def bridge_host_uart(db):
    """P4 host-link reads that saw one or more line errors or overflows over the capture: host
    bytes lost or corrupted.

    A P4 count, so it rebases only when it falls, as `BridgeLoss::host_uart_errors` does. The P4
    loses host bytes while it erases its own flash, which `flash-bridge` makes it do, and one fan
    duty change in 1024.
    """
    rows = db.q("SELECT host_uart_errors FROM bridge_status ORDER BY id")
    if not rows:
        return
    total = sum(since_boot_delta(cur[0], prev[0], False) for prev, cur in zip(rows, rows[1:]))
    print(f"  host link  host_uart_errors {total}")
    if total:
        flag("WARN", "bridge",
             f"{total} P4 host-link reads saw line errors or overflows: host frames were lost "
             "or corrupted "
             "on the way in, which nothing but the P4's own flash work should cause",
             "SELECT id, rx_at, uptime_ms, host_uart_errors FROM bridge_status ORDER BY id")


def bridge_fan(db):
    """The carrier's lid fan over the capture: the duties it ran and the speeds it read.

    Both columns are NULL with no carrier, and `fan_rpm` before the bridge's first tach window
    and after a duty change, until the first window that began after it.
    """
    rows = db.q("SELECT fan_duty, fan_rpm FROM bridge_status WHERE fan_duty IS NOT NULL")
    if not rows:
        print("  fan  none reported (no carrier)")
        return
    duties = [r[0] for r in rows]
    rpms = [r[1] for r in rows if r[1] is not None]
    print(f"  fan  duty {min(duties)}..{max(duties)}% ({len(set(duties))} distinct)  "
          f"rpm {spread(rpms)}")
    # 20 % is `FanDuty::TURNS_FROM`: below it the fan may stop by design.
    stopped = sum(1 for duty, rpm in rows if duty >= 20 and rpm == 0)
    if stopped:
        flag("WARN", "bridge",
             f"fan stopped in {stopped} status row(s): no tach pulse at 20% or more, so it "
             "was unplugged or stalled",
             "SELECT id, rx_at, uptime_ms, fan_duty, fan_rpm FROM bridge_status "
             "WHERE fan_duty >= 20 AND fan_rpm = 0 ORDER BY id")


def bridge_slots(db):
    """Each slot that went off the bus after it was first present: when, and for how long.

    `slots_present` is the P4's mask, bit n for the slot whose byte is n. A slot is absent after
    five failed transfers over at least a second, and the engine drops its node from the plan on
    that `Status` (`on_bus`). A bridge restart (`uptime_ms` falls) drops every slot at once, so an
    absence that begins on the first row after one is the bridge's, not its node's.
    """
    rows = db.q("SELECT rx_at, uptime_ms, slots_present FROM bridge_status ORDER BY id")
    if not rows:
        return
    first = [label(slot_name(k)) for k in range(MAX_SLOTS) if rows[0][2] >> k & 1]
    last = [label(slot_name(k)) for k in range(MAX_SLOTS) if rows[-1][2] >> k & 1]
    print(f"  slots  first {' '.join(first) or 'none'}  last {' '.join(last) or 'none'}")
    restarts = {i for i in range(1, len(rows)) if rows[i][1] < rows[i - 1][1]}
    restart_spells = set()  # row indices of restarts that some slot's absence began on
    for k in range(MAX_SLOTS):
        seen = False
        gone = None  # row index the current absence began on
        spells = []  # (row index it began on, ms or None while still absent at the end)
        for i, (at, _, mask) in enumerate(rows):
            present = bool(mask >> k & 1)
            if present and gone is not None:
                spells.append((gone, at - rows[gone][0]))
                gone = None
            elif not present and seen and gone is None:
                gone = i
            seen = seen or present
        if gone is not None:
            spells.append((gone, None))
        restart_spells |= {i for i, _ in spells if i in restarts}
        spells = [(i, ms) for i, ms in spells if i not in restarts]
        if not spells:
            continue
        slot = label(slot_name(k))
        for i, ms in spells[:5]:
            span = f"for {dur(ms)}" if ms is not None else "to the end"
            print(f"  slot {slot} off the bus at {rel(rows[i][0])} {span}")
        if len(spells) > 5:
            print(f"  slot {slot} off the bus {len(spells) - 5} more time(s)")
        flag("WARN", "bridge",
             f"slot {slot} dropped off the bus {len(spells)} time(s) mid-capture: its node lost "
             "power, crashed or was unplugged, or the host power-cycled it",
             f"SELECT id, rx_at, uptime_ms, slots_present FROM bridge_status "
             f"WHERE (slots_present >> {k}) & 1 = 0 ORDER BY id")
    for i in sorted(restart_spells):
        print(f"  bridge restart at {rel(rows[i][0])}: all slots absent until their nodes "
              "answered")


def host(db, ctx):
    section("host status")
    order = ["at", "frames", "duplicate_batches", "garbled", "undecodable", "incompatible",
             "lag_peak_us", "store_written", "store_dropped", "store_queue_peak",
             "store_commit_peak_us", "throttled", "soc_temp_mc", "battery_mv", "battery_ma"]
    rows = [dict(zip(order, r)) for r in db.q(f"SELECT {', '.join(order)} FROM host_status "
                                               "ORDER BY id")]
    if not rows:
        print("  no rows")
        return
    first, last = rows[0], rows[-1]
    deltas = {}
    for c in ("frames", "duplicate_batches", "garbled", "undecodable", "incompatible",
              "store_written", "store_dropped"):
        deltas[c] = last[c] - first[c]
    print("  " + "  ".join(f"{k} {v}" for k, v in deltas.items()))
    for c in ("lag_peak_us", "store_queue_peak", "store_commit_peak_us"):
        print(f"  {c:<22}{spread([r[c] for r in rows])}")
    behind = sum(1 for r in rows if r["lag_peak_us"] >= 100_000)
    if behind:
        flag("WARN" if behind > 5 else "INFO", "host",
             f"host read the link >=100 ms behind the bridge in {behind} of {len(rows)} samples "
             "(heartbeats read then are history)",
             "SELECT at, lag_peak_us FROM host_status WHERE lag_peak_us >= 100000")
    for c, sev in (("store_dropped", "WARN"), ("garbled", "WARN"), ("undecodable", "WARN"),
                   ("incompatible", "WARN")):
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
    words = [r["throttled"] for r in rows if r["throttled"] is not None]
    if words:
        under = sum(1 for w in words if w & 1)
        thr = sum(1 for w in words if w & 4)
        print(f"  pi throttle: under-voltage now in {under}, throttled now in {thr} of "
              f"{len(words)}; since-boot bits {words[-1] >> 16:#06b}")
        if under:
            flag("WARN", "power", f"Pi reported under-voltage in {under} samples",
                 "SELECT at, throttled FROM host_status WHERE throttled & 1")
    temps = [r["soc_temp_mc"] for r in rows if r["soc_temp_mc"] is not None]
    if temps:
        print(f"  soc temp C  max {max(temps) / 1000:.1f}  p50 "
              f"{statistics.median(temps) / 1000:.1f}")
        if max(temps) > 80_000:
            flag("WARN", "power", f"SoC reached {max(temps) / 1000:.1f} C")
    mv = [r["battery_mv"] for r in rows if r["battery_mv"] is not None]
    if mv:
        print(f"  battery V  first {mv[0] / 1000:.2f}  last {mv[-1] / 1000:.2f}  "
              f"min {min(mv) / 1000:.2f}")
    ma = [r["battery_ma"] for r in rows if r["battery_ma"] is not None]
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
    for mac, n, distinct, lo, hi in sorted(per_node, key=lambda r: node_order(r[0])):
        if not wanted(ctx, mac):
            continue
        chans = [c for (c,) in db.q("SELECT DISTINCT channel FROM observation WHERE node_mac=? "
                                    "ORDER BY 1", (mac,))]
        print(f"    {name(mac)}  {n}  {distinct}  {rel(lo)}..{rel(hi)}  ch {chans}")
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
             "SELECT MIN(rx_at) - (SELECT started_at FROM capture) FROM observation")
    heartbeating = set(ctx.get("hb", {}))
    for mac in sorted(heartbeating - {r[0] for r in per_node}, key=node_order):
        if mac not in ctx.get("ble_nodes", set()):
            flag("WARN", "sightings",
                 f"{name(mac)} heartbeated but stored no observations")
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
        print(f"  {name(mac)} stored no sighting for {dur(gap)} before {rel(rx)} while sending "
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
    off = db.q(f"SELECT channel, COUNT(*) FROM observation WHERE kind='wifi' AND channel NOT IN "
               f"({','.join(map(str, SCAN_CHANNELS))}) GROUP BY 1")
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
    total = db.one("SELECT COUNT(*) FROM observation") or 0
    mix = dict(db.q("SELECT pos_source, COUNT(*) FROM observation GROUP BY 1"))
    print("  sources " + "  ".join(f"{k} {v} ({pct(v, total)})" for k, v in mix.items()))
    if mix.get("none", 0) > 0.05 * total:
        flag("WARN", "gps", f"{pct(mix['none'], total)} of observations have no position",
             "SELECT (rx_at - (SELECT started_at FROM capture))/60000 m, COUNT(*) FROM observation "
             "WHERE pos_source='none' GROUP BY 1")
    if mix.get("static"):
        flag("WARN", "gps", "static positions present: test data, never uploadable")
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
    rows = db.q("SELECT uploaded_at, job_id, rows, through_id, result FROM upload ORDER BY id")
    if not rows:
        print("  none")
        return
    total = db.one("SELECT MAX(id) FROM observation")
    for at, job, n, through, result in rows:
        print(f"  job {job}  {utc(at)} ({rel(at)})  {n} rows  through observation {through} "
              f"of {total}  result {result}")
        if result == "failed":
            flag("WARN", "upload",
                 f"upload job {job} failed: the site imported nothing, so it is no cutoff and its "
                 f"{n} rows are still unsent",
                 "SELECT * FROM upload")
        elif result in (None, "unfollowed"):
            flag("INFO", "upload",
                 f"upload job {job} result is {result!r}: whether the site imported it is not "
                 "recorded (check the job on the site; never re-upload without asking)",
                 "SELECT * FROM upload")
    cutoffs = [r[3] for r in rows if r[4] != "failed"]
    if not cutoffs:
        return
    junk = db.one(
        "SELECT COUNT(DISTINCT bssid) FROM observation WHERE kind='wifi' AND id <= ? AND "
        "(bssid = zeroblob(6) OR instr('13579BDF', substr(hex(bssid), 2, 1)) > 0)",
        (cutoffs[-1],))
    if junk:
        print(f"  {junk} zero/group BSSIDs lie inside the uploaded range; the current export "
              "drops them, but compare the uploaded row count with `wartui export` to see whether "
              "the build that uploaded did")


# Mirrors bus::UpType in crates/wartui-proto/src/bus/msg.rs.
UP_TYPES = {1: "heartbeat", 2: "sightings", 3: "update status"}


def raw_frames(db, ctx):
    section("raw frames")
    n = db.one("SELECT COUNT(*) FROM raw_frame")
    print(f"  rows {n}")
    if not n:
        return
    by_slot = db.q("SELECT slot, kind, COUNT(*) FROM raw_frame GROUP BY 1, 2 ORDER BY 1, 2")
    for slot, kind, c in by_slot:
        print(f"    {label(slot)}  {UP_TYPES.get(kind, f'type {kind}')}  {c}")
    # Equal counts mean any heartbeat loss happened before the host read the record.
    raw_hb = dict(db.q("SELECT slot, COUNT(*) FROM raw_frame WHERE kind = 1 GROUP BY 1"))
    stored = Counter(h[5] for hs in ctx.get("hb", {}).values() for h in hs)
    if raw_hb:
        slots = sorted(set(raw_hb) | set(stored))
        print("  heartbeat records raw/stored per slot: " + "  ".join(
            f"{label(s)} {raw_hb.get(s, 0)}/{stored.get(s, 0)}" for s in slots))


LOG_LINE = re.compile(
    r"^(?P<ts>\d{4}-\d\d-\d\dT[\d:.]+Z?)\s+(?P<level>TRACE|DEBUG|INFO|WARN|ERROR)\s+"
    r"(?P<target>[\w:]+):\s?(?P<msg>.*)$")
KEYWORDS = re.compile(r"reconnect|identify|stall|reset|reboot|disconnect|timed? ?out|panic|"
                      r"wedge|Ready|uptime|gps|nmea|dropped|refused|error", re.I)


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
    silent = sum(c for (_, _, tpl), c in templates.items()
                 if "the radio has stopped answering" in tpl)
    if silent:
        flag("WARN", "log", f"the bridge said its radio stopped answering {silent} time(s)")
    moments = [(at, "bridge reboot") for at in ctx.get("bridge_reboots", [])]
    moments += [(r[0], f"{name(r[1])} restart") for r in ctx.get("restarts", [])[:6]]
    for at, what in moments:
        near = [e for e in events if e[0] and abs(e[0] - at) <= 60_000]
        print(f"  within 60 s of {what} at {rel(at)}: {len(near)} notable lines")
        for e in near[:8]:
            print(f"    {rel(e[0])}  {e[1]}  {e[2]}: {e[3][:140]}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("db")
    ap.add_argument("--log", help="the --log-file from the same run")
    ap.add_argument("--node", help="limit per-node detail to one node, by slot label or last two "
                                   "octets")
    args = ap.parse_args()
    db = Db(args.db)
    found = db.one("PRAGMA user_version")
    if found != SCHEMA_VERSION:
        sys.exit(f"{args.db} is schema version {found}, and this survey reads {SCHEMA_VERSION} "
                 "only: survey it with the wartui build that wrote it")
    ctx = {"only": args.node.upper() if args.node else None}
    print(f"survey of {args.db} (read-only)")
    print(f"tables: {', '.join(sorted(db.tables))}")
    provenance(db, ctx)
    # The Bluetooth node, read early so nodes() and heartbeats() can tell it from the sweepers.
    ctx["ble_nodes"] = {m for (m,) in db.q("SELECT DISTINCT node_mac FROM assignment "
                                            "WHERE ble=1 AND node_mac IS NOT NULL")}
    nodes(db, ctx)
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
