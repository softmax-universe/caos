# usage: python3 fsyncprobe.py <dir> [n]
# Time n (default 30) rounds of: write 300 KB, fsync, rename — the shape of
# podman's and git's metadata writes. Prints median / p90 / max in ms.
import os, sys, time

d = sys.argv[1]
n = int(sys.argv[2]) if len(sys.argv) > 2 else 30
f = os.path.join(d, 'fsync-probe')
lat = []
for _ in range(n):
    t = time.time()
    with open(f, 'wb') as h:
        h.write(b'x' * 300000)
        h.flush()
        os.fsync(h.fileno())
    os.rename(f, f + '.2')
    lat.append((time.time() - t) * 1000)
os.remove(f + '.2')
lat.sort()
print(f"300KB write+fsync+rename x{n}: median {lat[n // 2]:.0f}ms "
      f"p90 {lat[int(n * .9)]:.0f}ms max {lat[-1]:.0f}ms")
