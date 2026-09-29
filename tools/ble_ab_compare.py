#!/usr/bin/env python3
"""Compare the addresses two single-mode bench boards heard over the same stretch.

Each argument is a log from `espflash monitor` piped through a host timestamp
(`<unix seconds> <line>`). Only the last boot in each log counts, and only lines
inside the time both logs cover.
"""
import re, sys

def load(path):
    lines = open(path, errors="replace").read().splitlines()
    boots = [i for i, l in enumerate(lines) if "bench: boot modes=" in l]
    lines = lines[boots[-1]:] if boots else lines
    mode = re.search(r"modes=(\S+)", lines[0]).group(1) if boots else "?"
    heard, never_legacy = {}, set()
    for l in lines:
        m = re.match(r"(\d+\.\d+) bench: new .*addr=(\S+) legacy=(\w+)", l)
        if m:
            heard[m.group(2)] = float(m.group(1))
            if m.group(3) == "false":
                never_legacy.add(m.group(2))
        m = re.match(r"\d+\.\d+ bench: legacy .*addr=(\S+)", l)
        if m:
            never_legacy.discard(m.group(1))
    times = [float(l.split()[0]) for l in lines if re.match(r"\d+\.\d+ ", l)]
    return mode, heard, never_legacy, (min(times), max(times))

a, b = (load(p) for p in sys.argv[1:3])
lo, hi = max(a[3][0], b[3][0]), min(a[3][1], b[3][1])
A = {k for k, t in a[1].items() if lo <= t <= hi}
B = {k for k, t in b[1].items() if lo <= t <= hi}
print(f"overlap {hi - lo:.0f} s")
for name, mode, mine, other, nl in (("A", a[0], A, B, a[2]), ("B", b[0], B, A, b[2])):
    print(f"{name} {mode}: distinct {len(mine)}, only here {len(mine - other)}, never legacy {len(nl & mine)}")
print(f"both {len(A & B)}, union {len(A | B)}")
