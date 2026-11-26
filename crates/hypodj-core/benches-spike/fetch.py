#!/usr/bin/env nix-shell
#! nix-shell -i python3 -p python3
# Spike: dump the real library name index (read-only) to a json file. Creds never printed.
import tomllib,hashlib,json,sys,urllib.request,urllib.parse,secrets
c=tomllib.load(open('/run/user/1000/hypodj/config.toml','rb'))['server']
def call(ep,**p):
    s=secrets.token_hex(6)
    q=dict(u=c['username'],t=hashlib.md5((c['password']+s).encode()).hexdigest(),s=s,v='1.16.1',c='w28spike',f='json');q.update(p)
    r=urllib.request.urlopen(c['url'].rstrip('/')+'/rest/'+ep+'?'+urllib.parse.urlencode(q),timeout=60)
    return json.load(r)['subsonic-response']
out={'artists':[],'albums':[],'songs':[]}
for ix in call('getArtists')['artists'].get('index',[]):
    for a in ix['artist']: out['artists'].append({'id':a['id'],'name':a['name']})
off=0
while True:
    al=call('getAlbumList2',type='alphabeticalByName',size=500,offset=off)['albumList2'].get('album',[])
    if not al:break
    out['albums']+= [{'id':a['id'],'name':a['name'],'artist':a.get('artist','')} for a in al]; off+=500
off=0
while True:
    ss=call('search3',query='""',songCount=500,songOffset=off,artistCount=0,albumCount=0).get('searchResult3',{}).get('song',[])
    if not ss:break
    out['songs']+=[{'id':s['id'],'name':s['title'],'artist':s.get('artist',''),'album':s.get('album','')} for s in ss]; off+=500
json.dump(out,open(sys.argv[1],'w'))
print({k:len(v) for k,v in out.items()})
