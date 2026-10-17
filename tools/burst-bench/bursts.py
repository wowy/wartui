# Per-burst receive counts and seq gaps from a `sniff --verbose` capture (unstamped).
import sys,re
rows=[]
for line in open(sys.argv[1]):
    m=re.match(r'\s*(\d+)us\s+(\S+)\s+\S+\s+unicast\s+B#(\d+) 1/',line)
    if m: rows.append((int(m[1]),m[2][-5:],int(m[3])))
# group into bursts: per node, frames within 300 ms of previous
from collections import defaultdict
per=defaultdict(list)
for t,n,s in rows: per[n].append((t,s))
bursts=[]
for n,l in per.items():
    cur=[l[0]]
    for a in l[1:]:
        if a[0]-cur[-1][0]>300_000: bursts.append((n,cur)); cur=[a]
        else: cur.append(a)
    bursts.append((n,cur))
bursts.sort(key=lambda b:b[1][0][0])
for n,b in bursts[:40]:
    got=len(b); span=(b[-1][0]-b[0][0])/1e3; sent=(b[-1][1]-b[0][1])%65536+1
    # positions of gaps as fraction of burst
    gaps=[]
    for i in range(1,len(b)):
        d=(b[i][1]-b[i-1][1])%65536
        if d>1: gaps.append(f"{i}:{d-1}")
    print(f"{b[0][0]/1e6:8.2f}s {n} got {got:3}/{sent:3} over {span:6.0f} ms ({got/span*1e3 if span else 0:5.0f}/s recv) gaps {' '.join(gaps[:12])}")
