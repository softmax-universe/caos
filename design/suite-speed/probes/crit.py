import json,sys
t=json.load(open(sys.argv[1])); want=sys.argv[2]
def find(n):
    if n.get('name')==want and not n.get('reused'): return n
    for c in n.get('children',[]):
        r=find(c)
        if r: return r
n=find(t); d=0
while n:
    r,s,e=n.get('requested',0),n.get('started',0),n.get('ended',0)
    cs=[c for c in n.get('children',[]) if not c.get('reused')]
    own=(e-s)-max([0]+[c['ended']-c['started'] for c in cs])
    print(f"{'  '*d}{n['name'][:40]:40} start {s/1000:7.1f} run {(e-s)/1000:6.1f} kids {len(cs)}")
    n=max(cs,key=lambda c:c['ended']-c['started']) if cs else None; d+=1
