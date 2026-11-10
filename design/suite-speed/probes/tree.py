import json,sys
t=json.load(open(sys.argv[1])); want=sys.argv[2] if len(sys.argv)>2 else None
maxd=int(sys.argv[3]) if len(sys.argv)>3 else 99
def find(n):
    if n.get('name')==want: return n
    for c in n.get('children',[]):
        r=find(c)
        if r: return r
def show(n,d=0):
    if d>maxd: return
    r,s,e=n.get('requested',0),n.get('started',0),n.get('ended',0)
    print(f"{'  '*d}{n['name'][:40]:40} req {r/1000:7.1f} wait {(s-r)/1000:6.1f} run {(e-s)/1000:6.1f}{' R' if n.get('reused') else ''}")
    for c in n.get('children',[]): show(c,d+1)
show(find(t) if want else t)
