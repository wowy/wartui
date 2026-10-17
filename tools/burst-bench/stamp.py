# Prefix every line on stdin with the host's monotonic time in µs, writing to argv[1].
import sys,time
out=open(sys.argv[1],'w')
for line in sys.stdin:
    out.write(f"{time.monotonic_ns()//1000} {line}")
