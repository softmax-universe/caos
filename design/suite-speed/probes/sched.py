import json,sys
t=json.load(open(sys.argv[1]))
def find(n,name):
    if n.get('name')==name and not n.get('reused'): return n
    for c in n.get('children',[]):
        r=find(c,name)
        if r: return r
names=sys.argv[2].split()
rows=[]
for nm in names:
    n=find(t,nm)
    if n: rows.append((n['started']/1000,(n['ended'])/1000,nm))
for s,e,nm in sorted(rows,key=lambda r:r[1])[-15:]: print(f"{nm:24} start {s:6.1f} end {e:6.1f} dur {e-s:6.1f}")
