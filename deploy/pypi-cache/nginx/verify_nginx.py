#!/usr/bin/env python3
"""Read-only checks against a staged or public Shadowmire/Yukina endpoint."""
import argparse
import hashlib
import http.client
import json
from pathlib import Path
from urllib.parse import urlsplit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', required=True, help='For example http://127.0.0.1:18081 (without /pypi)')
    parser.add_argument('--host', help='Optional HTTP Host header for a staged virtual host')
    parser.add_argument('--storage', type=Path, required=True, help='Local Shadowmire root, e.g. /data/pypi')
    args = parser.parse_args()
    base = urlsplit(args.base_url)
    if base.scheme not in ('http', 'https') or base.username or base.password or base.path not in ('', '/'):
        parser.error('--base-url must be an HTTP(S) origin without credentials or path')

    def request(path, method='GET', headers=None, limit=2 * 1024 * 1024):
        cls = http.client.HTTPSConnection if base.scheme == 'https' else http.client.HTTPConnection
        conn = cls(base.hostname, base.port, timeout=30)
        extra = {'User-Agent': 'Synora-PyPI-validation/1'}
        if args.host:
            extra['Host'] = args.host
        extra.update(headers or {})
        conn.request(method, path, headers=extra)
        response = conn.getresponse()
        body = response.read(limit + 1)
        if len(body) > limit:
            raise AssertionError('Response exceeds validation limit: ' + path)
        result = response.status, {key.lower(): value for key, value in response.getheaders()}, body
        conn.close()
        return result

    def expect(path, code=200, method='GET', headers=None):
        result = request(path, method, headers)
        assert result[0] == code, (path, result[0], code)
        return result

    for suffix, mime in [('html', 'text/html'), ('v1_html', 'application/vnd.pypi.simple.v1+html'), ('v1_json', 'application/vnd.pypi.simple.v1+json')]:
        assert (args.storage / 'simple' / ('index.' + suffix)).is_file(), 'Missing root index ' + suffix
        _, headers, _ = expect('/pypi/simple/index.' + suffix, method='HEAD')
        assert headers.get('content-type', '').split(';')[0] == mime, headers
        _, headers, _ = expect('/pypi/simple/', method='HEAD', headers={'Accept': mime})
        assert headers.get('content-type', '').split(';')[0] == mime, headers
        assert 'accept' in headers.get('vary', '').lower(), headers
    for mime in ['text/html', 'application/vnd.pypi.simple.v1+html', 'application/vnd.pypi.simple.v1+json']:
        _, headers, body = expect('/pypi/simple/pip/', headers={'Accept': mime})
        assert headers.get('content-type', '').split(';')[0] == mime, headers
        if mime.endswith('+json'):
            assert isinstance(json.loads(body).get('files'), list)
    for path, target in [('/pypi/simple/PIP', '/pypi/simple/pip/'),
                         ('/pypi/simple/Some...___---Package/', '/pypi/simple/some-package/'),
                         ('/pypi/PIP/json', '/pypi/json/pip'),
                         ('/pypi/json/PIP', '/pypi/json/pip'),
                         ('/pypi/web/simple/pip/', '/pypi/simple/pip/')]:
        _, headers, _ = expect(path, 301)
        assert urlsplit(headers['location']).path == target, (path, headers)
    _, headers, body = expect('/pypi/json/pip')
    assert headers.get('content-type', '').split(';')[0] == 'application/json'
    assert json.loads(body)['info']['name'].lower() == 'pip'
    for path in ['/pypi/packages/ab/cd/1234/example.whl.tmp', '/pypi/local.db', '/pypi/local.json', '/pypi/.yukina/private', '/pypi/simple/synora-validation-absent-project-84d7a42/']:
        expect(path, 404)
    miss = '/pypi/packages/00/00/' + '0' * 60 + '/synora-validation-absent.whl'
    _, headers, _ = expect(miss, 302)
    assert headers['location'] == 'https://mirrors.tuna.tsinghua.edu.cn/pypi/web/packages/' + miss.split('/packages/', 1)[1]
    expect('/pypi/simple/', 406, headers={'Accept': 'image/png'})
    package = next((path for path in (args.storage / 'packages').glob('*/*/*/*')
                    if path.is_file() and not path.name.endswith('.tmp') and path.stat().st_size >= 16), None)
    assert package is not None, 'No cached package available for HEAD/Range validation'
    path = '/pypi/packages/' + package.relative_to(args.storage / 'packages').as_posix()
    _, headers, _ = expect(path, method='HEAD')
    assert int(headers['content-length']) == package.stat().st_size
    end = min(package.stat().st_size, 65536) - 1
    _, headers, content = expect(path, 206, headers={'Range': f'bytes=0-{end}'})
    with package.open('rb') as handle:
        local = handle.read(end + 1)
    assert hashlib.sha256(content).digest() == hashlib.sha256(local).digest(), 'Cached package range hash mismatch'
    assert headers.get('content-range') == f'bytes 0-{end}/{package.stat().st_size}'
    print('PASS: root index HEAD, pip HTML/JSON, canonical names, legacy route, hidden state, cache miss and cached package HEAD/Range hash')


if __name__ == '__main__':
    main()
