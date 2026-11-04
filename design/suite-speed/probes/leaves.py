import json,sys
t=json.load(open(sys.argv[1]))
seen={}
def walk(n):
    cs=n.get('children',[])
    if not cs and not n.get('reused') and n['ended']>n['started']:
        seen[n['arg_tree']]=(n['started']/1000,(n['ended']-n['started'])/1000,n['name'])
    for c in cs: walk(c)
walk(t)
L=sorted(seen.values())
print("leaves",len(L))
import statistics
for lo in range(0,int(L[-1][0])+60,60):
    b=[x[1] for x in L if lo<=x[0]<lo+60]
    if b: print(f"t={lo:4d}-{lo+60:4d}s n={len(b):4d} median={statistics.median(b):5.1f}s p90={sorted(b)[int(len(b)*.9)]:5.1f}s")
print("total leaf seconds", sum(x[1] for x in L))
for x in sorted(L,key=lambda x:-x[1])[:12]: print(x)
