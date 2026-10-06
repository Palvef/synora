"""Validate access logs before allowing a popularity-cache garbage collection."""
import datetime
import gzip
import ipaddress
import json
import math
import re
import time
from pathlib import Path
from urllib.parse import unquote, urlsplit

LEGACY = re.compile(r'^(\S+) \S+ \S+ \[([^]]+)\] "([^"]+)" (\d+) (\d+) "(?:[^"\\]|\\.)*" "(?:[^"\\]|\\.)*" "((?:[^"\\]|\\.)*)"(?: .*)?$')

def normalize(item):
    ipaddress.ip_address(item['clientip'])
    stamp=float(item['timestamp'])
    if not math.isfinite(stamp) or stamp <= 0: raise ValueError('invalid timestamp')
    size=int(item['size']); status=int(item['status'])
    if size < 0 or not 100 <= status <= 599: raise ValueError('invalid response')
    if not isinstance(item['user_agent'],str): raise ValueError('invalid user agent')
    uri=urlsplit(item['url']).path
    for _ in range(3):
        decoded=unquote(uri)
        if '\\' in decoded or '\x00' in decoded or any(p in ('.','..') for p in decoded.split('/')):
            raise ValueError('unsafe path')
        if decoded==uri: break
        uri=decoded
    if '%' in uri: raise ValueError('ambiguous escaped path')
    uri=re.sub('/+', '/', uri)
    if uri in ('/pypi', '/pypi/web'): uri='/pypi/'
    if uri.startswith('/pypi/web/'): uri='/pypi/'+uri[len('/pypi/web/'):]
    if not uri.startswith('/pypi/'): raise ValueError('unexpected repo in dedicated log')
    proxied=str(item.get('proxied',''))
    if proxied not in ('0','1'): raise ValueError('missing/invalid cache origin')
    return dict(timestamp=stamp,clientip=item['clientip'],url=uri,size=size,status=status,user_agent=item['user_agent'],proxied=proxied)

def legacy(line):
    m=LEGACY.fullmatch(line.rstrip('\n'))
    if not m: raise ValueError('invalid legacy access log')
    ip,stamp,request,status,size,agent=m.groups()
    parts=request.split()
    if len(parts)!=3: raise ValueError('invalid request')
    return normalize(dict(clientip=ip,timestamp=datetime.datetime.strptime(stamp,'%d/%b/%Y:%H:%M:%S %z').timestamp(),url=parts[1],status=status,size=size,user_agent=agent,proxied='1'))

def prepare(source, historical, output, now=None):
    now=time.time() if now is None else now
    cutoff=now-7*86400
    count=0
    tmp=output.with_suffix('.tmp')
    try:
        with tmp.open('w') as dest:
            for root,old in ((source,False),(historical,True)):
                if root is None or not root.exists(): continue
                for path in sorted(root.rglob('pypi*.log*')):
                    if not path.is_file() or path.is_symlink() or path.stat().st_mtime < cutoff: continue
                    opener=gzip.open if path.suffix=='.gz' else open
                    with opener(path,'rt',encoding='utf-8',errors='strict') as stream:
                        for number,line in enumerate(stream,1):
                            if not line.strip(): continue
                            try: item=legacy(line) if old else normalize(json.loads(line))
                            except (ValueError,KeyError,TypeError) as exc:
                                raise ValueError(f'invalid access log {path.name}:{number}') from exc
                            if cutoff <= item['timestamp'] <= now+300:
                                dest.write(json.dumps(item,separators=(',',':'))+'\n'); count+=1
            if not count: raise ValueError('no valid access requests in the last seven days; refusing cache GC')
        tmp.replace(output)
    finally:
        tmp.unlink(missing_ok=True)
    return count
