#!/usr/bin/env nix-shell
#! nix-shell -i python3 -p python3
# Spike: measure search3 tolerance + 3 fuzzy approaches on the real library. Read-only.
# usage: bench.py lib.json   (lib.json from fetch.py). Creds read via tomllib, never printed.
import tomllib,hashlib,json,sys,urllib.request,urllib.parse,secrets,unicodedata,re,time,tracemalloc
lib=json.load(open(sys.argv[1]))
c=tomllib.load(open('/run/user/1000/hypodj/config.toml','rb'))['server']
def s3(q):
    s=secrets.token_hex(6)
    p=dict(u=c['username'],t=hashlib.md5((c['password']+s).encode()).hexdigest(),s=s,v='1.16.1',c='w28spike',f='json',query=q,artistCount=20,albumCount=50,songCount=200)
    r=urllib.request.urlopen(c['url'].rstrip('/')+'/rest/search3?'+urllib.parse.urlencode(p),timeout=30)
    r=json.load(r)['subsonic-response'].get('searchResult3',{})
    return {'artists':r.get('artist',[]),'albums':r.get('album',[]),'songs':r.get('song',[])}
NAME={'artists':'name','albums':'name','songs':'title'}
# (query, kind, exact intended name, flaw class)
Q=[("bjork","artists","Björk","accent"),("bjrk","artists","Björk","drop+accent"),("foehn","artists","Föhn","translit"),
("fohn","artists","Föhn","accent"),("roman fluegel","artists","Roman Flügel","translit"),("roman flugel","artists","Roman Flügel","accent"),
("vig mihaly","artists","Víg Mihály","accent"),("masive attack","artists","Massive Attack, Mos Def","typo"),
("amon tobbin","artists","Amon Tobin","typo"),("amon tobn","artists","Amon Tobin","drop"),("tosca","artists","Tosca","exact"),
("sakurazaka 46","artists","Sakurazaka46","split"),("rolling stone","artists","The Rolling Stones","partial"),
("rollingstones","artists","The Rolling Stones","space"),("simon turner","artists","Simon Fisher Turner","partial-words"),
("turner simon fisher","artists","Simon Fisher Turner","order"),("bonnie prince billy","artists","Bonnie 'Prince' Billy","punct"),
("life without buildngs","artists","Life Without Buildings","typo"),("pere ubuu","artists","Pere Ubu","typo"),
("dntl","artists","Dntel","drop"),("klangstoff","artists","Klangstof","typo"),("suzanne craft","artists","Suzanne Kraft","typo"),
("hunky dorry","albums","Hunky Dory","typo"),("parklif","albums","Parklife (Canadian Expanded Issue)","drop"),
("modern life is rubish","albums","Modern Life Is Rubbish (1993 Japanese Expanded Issue)","typo"),
("bananenrauber","albums","Bananenräuber EP","accent"),("any other citty","albums","Any Other City","typo"),
("i against i","albums","I Against I","exact"),("eleven numbrs","albums","Eleven Numbers","typo"),
("kesz az egesz","songs","Kész az egész","accent"),("hyperon","songs","Hyperion","typo"),
("feeling sleezy","songs","Feeling Sleazy","typo"),("could be nise","songs","Could Be Nice","typo"),
("today i was evil one","songs","Today I Was an Evil One","missing-word"),("dont loose the center","songs","Don’t Loose The Center feat. KAYYAK","punct"),
("pain a rolin away","songs","Pain A Rollin’Away","typo")]
def fold(s):
    s=unicodedata.normalize('NFKD',s.lower()); s=''.join(ch for ch in s if not unicodedata.combining(ch))
    for a,b in (('ü','ue'),):pass
    s=re.sub(r"[^\w]+"," ",s.replace('ß','ss')); s=re.sub(r"(?<=\d)(?=[a-z])|(?<=[a-z])(?=\d)"," ",s); return s.strip()
def foldde(s): # transliteration variant: ö->oe etc applied BEFORE fold
    return s.lower().replace('ö','oe').replace('ü','ue').replace('ä','ae').replace('ß','ss')
def tri(s):
    s=f"  {s} "; return {s[i:i+3] for i in range(len(s)-2)}
# --- local index
t0=time.perf_counter(); tracemalloc.start()
IDX={}
for k in NAME:
    IDX[k]=[]
    for it in lib[k]:
        f=fold(it['name']); IDX[k].append((it['name'],f,tri(f),tri(f.replace(' ',''))))
build=time.perf_counter()-t0; mem=tracemalloc.get_traced_memory()[0]; tracemalloc.stop()
def lev(a,b):
    p=list(range(len(b)+1))
    for i,x in enumerate(a,1):
        n=[i]
        for j,y in enumerate(b,1): n.append(min(p[j]+1,n[-1]+1,p[j-1]+(x!=y)))
        p=n
    return p[-1]
def score(q,e):
    qf=fold(q); _,f,t,tn=e
    if not qf: return 0
    qt=tri(qf); qn=tri(qf.replace(' ',''))
    j=max(len(qt&t)/len(qt|t), len(qn&tn)/len(qn|tn))
    # token-wise: each query token best-matches an entry token (typo tolerant, order free, partial words ok)
    ew=f.split(); qw=qf.split(); tot=0
    for w in qw:
        tot+=max((1-lev(w,x)/max(len(w),len(x)) if len(w)>2 else float(w==x)) for x in ew) if ew else 0
    return 0.5*j+0.5*tot/len(qw)
def local(q,k,n=5):
    sc=sorted(((score(q,e),e[0]) for e in IDX[k]),reverse=True)[:n]; return [x[1] for x in sc if x[0]>0.35]
def names(r,k): return [x[NAME[k]] for x in r[k]]
def rewrite(q):  # (b) variants only
    v=[q]; f=fold(q); g=fold(foldde(q))
    for x in (f,g):
        if x not in v: v.append(x)
    toks=f.split()
    if len(toks)>1 and ' '.join(toks) not in v:v.append(' '.join(toks))
    return v
def merge(lists):
    seen=[];[seen.append(x) for l in lists for x in l if x not in seen];return seen
def b_run(q,k):
    return merge([names(s3(v),k) for v in rewrite(q)])
def b2_run(q,k): # rewrite + prefix-truncate each token (typo in tail of word)
    vs=rewrite(q)
    f=fold(q).split()
    if len(f)>=1: vs.append(' '.join(w[:max(3,len(w)-2)] for w in f))
    return merge([names(s3(v),k) for v in vs])
def c_run(q,k,thresh=1):
    base=names(s3(q),k)
    if len(base)>=thresh and any(True for _ in base[:1]) and len(base)>=5: return base,False
    return merge([base,local(q,k)]),True
res={m:0 for m in 'base b b2 a c'.split()}; lat={m:[] for m in res}; calls={m:0 for m in res}
rows=[]
for q,k,want,cls in Q:
    def rk(l): 
        l=l[:5]; return want in l
    out={}
    for m,fn in (('base',lambda:names(s3(q),k)),('b',lambda:b_run(q,k)),('b2',lambda:b2_run(q,k)),('a',lambda:local(q,k)),('c',lambda:c_run(q,k)[0])):
        t=time.perf_counter(); l=fn(); lat[m].append((time.perf_counter()-t)*1000); out[m]=rk(l); res[m]+=out[m]
    rows.append((q,cls,out))
n=len(Q)
print("query | class | "+" ".join(res))
for q,cls,o in rows: print(f"{q!r} {cls}: "+" ".join('Y' if o[m] else '.' for m in res))
import statistics as st
print(f"N={n}")
for m in res: print(m,f"hits@5={res[m]}/{n}",f"lat median={st.median(lat[m]):.1f}ms max={max(lat[m]):.1f}ms")
print(f"index: {sum(len(v) for v in IDX.values())} names, build {build*1000:.0f}ms, python heap {mem/1e6:.1f}MB")
