# usage: syssample.py <dir> — every 0.2s, for every thread of every `server` process that is
# RUNNING, record the syscall it is in (or "user") and the thread name; histogram at exit.
import os, sys, time, collections
d = sys.argv[1]; h = collections.Counter(); names = collections.Counter()
while not os.path.exists(os.path.join(d, 'stop')):
    for p in os.listdir('/proc'):
        if not p.isdigit(): continue
        try:
            if open(f'/proc/{p}/comm').read().strip() != 'server': continue
            for t in os.listdir(f'/proc/{p}/task'):
                st = open(f'/proc/{p}/task/{t}/stat').read()
                state = st.rsplit(')', 1)[1].split()[0]
                if state != 'R': continue
                sc = open(f'/proc/{p}/task/{t}/syscall').read().split()[0]
                tn = open(f'/proc/{p}/task/{t}/comm').read().strip()
                h['user' if sc == 'running' else sc] += 1
                names[tn[:7]] += 1
        except Exception: pass
    time.sleep(0.2)
with open(os.path.join(d, 'sys.txt'), 'w') as f:
    for k, v in h.most_common(25): f.write(f'{v:7d} {k}\n')
    f.write('-- threads\n')
    for k, v in names.most_common(15): f.write(f'{v:7d} {k}\n')
