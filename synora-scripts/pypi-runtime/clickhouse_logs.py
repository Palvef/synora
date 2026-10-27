"""Stream validated cache votes from a read-only ClickHouse view."""
import base64
import json
import os
import re
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from logs import normalize, UnusableRequestPath

WINDOW = 7 * 86400
MAX_LINE_BYTES = 1024 * 1024


def configuration():
    path = os.environ.get('PYPI_CLICKHOUSE_CONFIG')
    if not path:
        raise ValueError('PYPI_CLICKHOUSE_CONFIG is required; local access logs are not used')
    try:
        value = json.loads(Path(path).read_text())
        endpoint = urllib.parse.urlsplit(value['endpoint'])
        if endpoint.scheme not in ('http', 'https') or not endpoint.hostname or endpoint.username or endpoint.password:
            raise ValueError()
        if endpoint.query or endpoint.fragment:
            raise ValueError()
        if not value.get('username') or not value.get('password'):
            raise ValueError()
        return value
    except (OSError, ValueError, KeyError, TypeError):
        raise ValueError('invalid ClickHouse configuration') from None


def query(config, now):
    table = config.get('table', 'mirrorlog.synora_pypi_access')
    if not isinstance(table, str) or not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*\.[A-Za-z_][A-Za-z_0-9]*', table):
        raise ValueError('invalid ClickHouse view name')
    return f"""SELECT timestamp, clientip, url, size, status, user_agent, proxied,
       count() OVER () AS total_requests
FROM {table}
WHERE timestamp >= {int(now-WINDOW)} AND timestamp <= {int(now)}
  AND (startsWith(url, '/pypi/packages/') OR startsWith(url, '/pypi/web/packages/'))
FORMAT JSONEachRow"""


def open_query(config, sql):
    token = base64.b64encode((config['username'] + ':' + config['password']).encode()).decode()
    url = config['endpoint'].rstrip('/') + '/?wait_end_of_query=1'
    request = urllib.request.Request(url, data=sql.encode(), headers={
        'Authorization': 'Basic ' + token, 'Content-Type': 'text/plain; charset=utf-8'})
    # Sync egress proxies must not intercept this internal database request.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    return opener.open(request, timeout=120)


def prepare(output, *, config=None, now=None, progress=None):
    config = configuration() if config is None else config
    now = time.time() if now is None else now
    sql = query(config, now)
    maximum_age = int(config.get('max_age_seconds', 3600))
    if maximum_age <= 0:
        raise ValueError('ClickHouse max_age_seconds must be positive')
    temporary = output.with_suffix('.tmp')
    total = None
    received = count = rejected = 0
    latest = 0
    last_report = time.monotonic()
    if progress:
        progress('ClickHouse: querying seven-day PyPI package requests')
    try:
        try:
            response = open_query(config, sql)
        except Exception:
            raise RuntimeError('ClickHouse access-log query failed; refusing cache GC') from None
        with response, temporary.open('w') as destination:
            while True:
                line = response.readline(MAX_LINE_BYTES+1)
                if not line:
                    break
                if len(line) > MAX_LINE_BYTES:
                    raise ValueError('oversized ClickHouse access record')
                try:
                    row = json.loads(line)
                    expected = int(row.pop('total_requests'))
                    if expected <= 0 or (total is not None and total != expected):
                        raise ValueError()
                    total = expected
                    received += 1
                    item = normalize(row)
                    if not now-WINDOW <= item['timestamp'] <= now:
                        raise ValueError()
                    if not item['url'].startswith('/pypi/packages/'):
                        raise ValueError()
                except UnusableRequestPath:
                    rejected += 1
                    continue
                except (ValueError, KeyError, TypeError, AttributeError):
                    raise ValueError(f'invalid ClickHouse access record {received}; refusing cache GC') from None
                latest = max(latest, item['timestamp'])
                destination.write(json.dumps(item, separators=(',', ':'))+'\n')
                count += 1
                if progress and time.monotonic()-last_report >= 15:
                    progress(f'ClickHouse: {received}/{total} records received; {count} validated')
                    last_report = time.monotonic()
            if not count or received != total:
                raise ValueError('empty or incomplete ClickHouse access logs; refusing cache GC')
            if latest < now-maximum_age:
                raise ValueError('stale ClickHouse access logs; refusing cache GC')
        temporary.replace(output)
        if progress:
            progress(f'ClickHouse: {count} package requests validated; {rejected} unsafe paths ignored')
        return count
    finally:
        temporary.unlink(missing_ok=True)
