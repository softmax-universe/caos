# usage: procacct.py <dir> — sample /proc every 0.5s until <dir>/stop; per-pid max utime/stime,
# aggregated at exit by a command key into <dir>/acct.txt
import os, sys, time, collections
d = sys.argv[1]; tck = os.sysconf('SC_CLK_TCK')
seen = {}  # (pid, starttime) -> [key, utime, stime, u0, s0]
first = True
def key_of(pid, comm):
    try:
        a = open(f'/proc/{pid}/cmdline','rb').read().split(b'\0')
        a = [x.decode(errors='replace') for x in a if x]
    except Exception: a = []
    if not a: return comm
    exe = os.path.basename(a[0])
    if exe in ('git',) and len(a) > 1:
        sub = next((x for x in a[1:] if not x.startswith('-') and '=' not in x and '/' not in x), '?')
        return f'git {sub}'
    if exe in ('bash','sh') and len(a) > 1: return f'{exe} {os.path.basename(a[1])}'
    if exe == 'caos' and len(a) > 1: return f'caos {a[1]}'
    if exe in ('rustc','clippy-driver','rustdoc') and '--crate-name' in a:
        cn = a[a.index('--crate-name')+1]
        where = 'dep' if any('cargo-package' in x or 'vendor-cargo-deps' in x for x in a) else 'ws'
        return f'{exe} {where} {cn}'
    return exe
while not os.path.exists(os.path.join(d,'stop')):
    for p in os.listdir('/proc'):
        if not p.isdigit(): continue
        try:
            s = open(f'/proc/{p}/stat').read()
            r = s.rsplit(')',1)[1].split()
            comm = s[s.index('(')+1:s.rindex(')')]
            ut, st, start = int(r[11]), int(r[12]), r[19]
        except Exception: continue
        k = (p, start)
        e = seen.get(k)
        if e is None: seen[k] = [key_of(p, comm), ut, st, ut if first else 0, st if first else 0]
        else: e[1], e[2] = ut, st
    first = False
    time.sleep(0.5)
agg = collections.defaultdict(lambda: [0,0,0])
for (p,_), (k,u,s,u0,s0) in seen.items():
    a = agg[k]; a[0] += (u-u0)/tck; a[1] += (s-s0)/tck; a[2] += 1
with open(os.path.join(d,'acct.txt'),'w') as f:
    for k,(u,s,n) in sorted(agg.items(), key=lambda x: -(x[1][0]+x[1][1])):
        f.write(f'{u:9.1f} {s:9.1f} {n:6d} {k}\n')
