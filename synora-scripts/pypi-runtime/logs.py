"""Validate access logs before allowing a popularity-cache garbage collection."""
import datetime
import gzip
import hashlib
import shutil
import ipaddress
import json
import math
import re
import time
from pathlib import Path
from urllib.parse import unquote, urlsplit

PROGRESS_INTERVAL = 15.0

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

def fingerprint(path):
    stat = path.stat()
    return [stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns]


def validated_file(path, old, cache, progress):
    # Include the format revision so normalization changes invalidate old caches.
    key = hashlib.sha256((str(path.resolve()) + str(old) + ':1').encode()).hexdigest()
    data = cache / (key + '.log')
    metadata = cache / (key + '.json')
    identity = fingerprint(path)
    try:
        info = json.loads(metadata.read_text())
        if info['identity'] == identity and data.stat().st_size == info['bytes']:
            if progress: progress(f'Access logs: reusing validated {path.name} ({info["count"]} requests)')
            return data, info, key
    except (OSError, ValueError, KeyError, TypeError):
        pass
    temporary = data.with_suffix('.tmp')
    count = 0
    earliest = float('inf')
    latest = 0
    last_report = time.monotonic()
    opener = gzip.open if path.suffix == '.gz' else open
    try:
        with opener(path, 'rt', encoding='utf-8', errors='strict') as stream, temporary.open('w') as dest:
            for number, line in enumerate(stream, 1):
                if not line.strip(): continue
                try: item = legacy(line) if old else normalize(json.loads(line))
                except (ValueError, KeyError, TypeError) as exc:
                    raise ValueError(f'invalid access log {path.name}:{number}') from exc
                stamp = item['timestamp']
                earliest = min(earliest, stamp); latest = max(latest, stamp)
                dest.write(json.dumps(item, separators=(',', ':')) + '\n')
                count += 1
                if progress and time.monotonic() - last_report >= PROGRESS_INTERVAL:
                    progress(f'Access logs: validating {path.name}; {count} requests processed')
                    last_report = time.monotonic()
        info = dict(identity=identity, count=count, earliest=earliest if count else 0,
                    latest=latest, bytes=temporary.stat().st_size)
        # Never publish a reusable cache for a source changing during validation.
        metadata.unlink(missing_ok=True)
        temporary.replace(data)
        if fingerprint(path) == identity:
            meta_tmp = metadata.with_suffix('.tmp')
            meta_tmp.write_text(json.dumps(info))
            meta_tmp.replace(metadata)
        return data, info, key
    finally:
        temporary.unlink(missing_ok=True)


def prepare(source, historical, output, now=None, progress=None):
    now = time.time() if now is None else now
    cutoff = now - 7*86400
    count = 0
    scanned = 0
    tmp = output.with_suffix('.tmp')
    cache = output.parent / '.validated-access-logs'
    cache.mkdir(exist_ok=True)
    used = set()
    try:
        with tmp.open('w') as dest:
            for root, old in ((source, False), (historical, True)):
                if root is None or not root.exists(): continue
                for path in sorted(root.rglob('pypi*.log*')):
                    if not path.is_file() or path.is_symlink() or path.stat().st_mtime < cutoff: continue
                    if progress:
                        progress(f'Access logs: reading {path.name}; {scanned} lines processed, {count} recent requests validated')
                    data, info, key = validated_file(path, old, cache, progress)
                    used.add(key)
                    scanned += info['count']
                    if info['count'] and cutoff <= info['earliest'] and info['latest'] <= now+300:
                        # Most immutable rotations fit entirely inside the window.
                        # Copy in chunks without JSON parsing or decompression.
                        with data.open() as stream: shutil.copyfileobj(stream, dest, 1024*1024)
                        count += info['count']
                    elif info['latest'] >= cutoff and info['earliest'] <= now+300:
                        with data.open() as stream:
                            for line in stream:
                                item = json.loads(line)
                                if cutoff <= item['timestamp'] <= now+300:
                                    dest.write(line); count += 1
                    if progress:
                        progress(f'Access logs: {scanned} lines processed, {count} recent requests validated; {path.name}')
            if not count: raise ValueError('no valid access requests in the last seven days; refusing cache GC')
        tmp.replace(output)
        # Drop caches for rotations no longer selected; storage stays bounded.
        for path in cache.iterdir():
            if path.stem not in used: path.unlink(missing_ok=True)
        return count
    finally:
        tmp.unlink(missing_ok=True)
