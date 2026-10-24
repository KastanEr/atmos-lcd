import sys,struct
from collections import Counter
def packets(path):
    f=open(path,'rb'); gh=f.read(24)
    assert struct.unpack('<I',gh[:4])[0]==0xa1b2c3d4
    while True:
        h=f.read(16)
        if len(h)<16: return
        ts,tus,incl,orig=struct.unpack('<IIII',h); d=f.read(incl)
        if len(d)<incl: return
        hl,irp,status,func,info,bus,dev,ep,xfer,dlen=struct.unpack('<HQIHBHHBBI',d[:27])
        yield dict(t=ts+tus/1e6,irp=irp,status=status,func=func,info=info,bus=bus,dev=dev,ep=ep,xfer=xfer,data=d[hl:],hdr=d[27:hl])
def text_of(d):
    if d[:1]==b'\x20': return d[4:4+((d[2]<<8)|d[3])]
    if d[:1]==b'\x1e': return d[1:].split(b'\0')[0]
    return d[:48]
if __name__=='__main__':
    path,mode=sys.argv[1],sys.argv[2]
    pk=packets(path); t0=None
    if mode=='summary':
        c=Counter()
        for p in pk: c[(p['dev'],hex(p['ep']),p['xfer'],p['info']&1,len(p['data'])>0, p['data'][:1].hex())]+=1
        for k,v in sorted(c.items()): print(k,v)
    elif mode=='log':
        lim=int(sys.argv[3]); n=0; last1f=None
        for p in pk:
            if t0 is None: t0=p['t']
            d=p['data']
            if not d: continue
            k=d[:1]
            if k==b'\x1f':
                hdr=d[:24].hex()
                if last1f is None: print(f"{p['t']-t0:9.3f} >> 1f data start hdr={hdr}")
                last1f=(p['t']-t0,hdr); continue
            if last1f: print(f"{last1f[0]:9.3f} >> 1f data end   hdr={last1f[1]}"); last1f=None
            if p['xfer']==2 and len(d)<64: print(f"{p['t']-t0:9.3f} ctrl {'<' if p['info']&1 else '>'} {d.hex()}"); continue
            print(f"{p['t']-t0:9.3f} {'<' if p['info']&1 else '>'} ep{p['ep']:#04x} {k.hex()} {text_of(d)[:600]!r}")
            n+=1
            if n>=lim: break
