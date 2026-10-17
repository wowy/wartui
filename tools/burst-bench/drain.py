# Drain rates from a stamped `sniff --verbose` capture: frames/s while frames arrive, and after.
# A frame that reached the bridge before the one ahead reached the host was queued; the
# spacing of such arrivals is the drain rate. See docs/link-encode-findings.md.
import sys,re
rows=[]
for line in open(sys.argv[1]):
    m=re.match(r'(\d+)\s+(\d+)us\s+(\S+)\s+\S+\s+unicast\s+B#(\d+) 1/(\d+)',line)
    if m: rows.append((int(m[1]),int(m[2])))
off=min(h-b for h,b in rows)
segs=[];cur=[rows[0]]
for r in rows[1:]:
    if r[0]-cur[-1][0]>20000: segs.append(cur);cur=[r]
    else: cur.append(r)
segs.append(cur)
bn=bd=qn=qd=0
for s in segs:
    if len(s)<40: continue
    last_rx=max(b for _,b in s)+off
    busy=[h for h,_ in s if h<=last_rx]; quiet=[h for h,_ in s if h>last_rx]
    if len(busy)>2: bn+=len(busy)-1; bd+=busy[-1]-busy[0]
    if len(quiet)>2: qn+=len(quiet)-1; qd+=quiet[-1]-quiet[0]
print(f"while receiving: {bn/bd*1e6:.0f} frames/s ({bn} frames)   after: {qn/qd*1e6:.0f} frames/s ({qn} frames)")
