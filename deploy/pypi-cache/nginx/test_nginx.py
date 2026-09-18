#!/usr/bin/env python3
"""Hermetic Nginx/njs integration smoke test; requires Docker, no production access."""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

HERE = Path(__file__).resolve().parent
IMAGE = os.environ.get('PYPI_NGINX_TEST_IMAGE', 'nginx:1.29-alpine')


def main():
    name = 'synora-pypi-test-' + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix='synora-pypi-nginx-') as temp:
        root = Path(temp)
        root.chmod(0o755)
        data = root / 'data'
        logs = root / 'logs'
        logs.mkdir()
        logs.chmod(0o777)
        fixtures = {
            'simple/index.html': '<html>root HTML</html>',
            'simple/index.v1_html': '<html>root vendor HTML</html>',
            'simple/index.v1_json': '{"projects":[]}',
            'simple/foo-bar/index.html': '<html>project</html>',
            'simple/foo-bar/index.v1_html': '<html>vendor project</html>',
            'simple/foo-bar/index.v1_json': '{"files":[]}',
            'json/foo-bar': '{"info":{"name":"foo-bar"}}',
            'simple/pip/index.html': '<html>pip</html>',
            'simple/pip/index.v1_html': '<html>pip</html>',
            'simple/pip/index.v1_json': '{"files":[]}',
            'json/pip': '{"info":{"name":"pip"}}',
            'packages/ab/cd/1234/example.whl': '01234567890123456789',
            'packages/ab/cd/1234/example.whl.tmp': 'DO NOT EXPOSE',
            'local.db': 'DO NOT EXPOSE',
            '.yukina/private': 'DO NOT EXPOSE',
        }
        for path, content in fixtures.items():
            dest = data / 'pypi' / path
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(content)
        config = root / 'nginx.conf'
        config.write_text('''load_module /usr/lib/nginx/modules/ngx_http_js_module.so;
events {}
http {
    include /etc/nginx/pypi-http.conf;
    map $http_x_test_forbidden $is_forbidden { default 0; "1" 1; }
    server {
        listen 80;
        server_name localhost;
        root /wrong-root;
        error_page 404 /404.html;
        error_page 418 = @forbidden;
        location @forbidden { return 403; }
        include /etc/nginx/pypi-server.conf;
    }
}
''')
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        args = ['docker', 'run', '--rm', '-d', '--name', name, '-p', f'127.0.0.1:{port}:80']
        for src, dst in [
            (config, '/etc/nginx/nginx.conf'),
            (HERE / 'http.conf', '/etc/nginx/pypi-http.conf'),
            (HERE / 'server.conf', '/etc/nginx/pypi-server.conf'),
            (HERE / 'common-static.conf', '/etc/nginx/snippets/synora-pypi-common.conf'),
            (HERE / 'synora-pypi.js', '/etc/nginx/njs/synora-pypi.js'),
            (data, '/data'),
        ]:
            args += ['-v', f'{src}:{dst}:ro']
        args += ['-v', f'{logs}:/var/log/nginx/pypi-cache', IMAGE]
        subprocess.run(args, check=True, stdout=subprocess.DEVNULL)
        try:
            def get(path, headers=None, method='GET'):
                conn = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
                conn.request(method, path, headers=headers or {})
                response = conn.getresponse()
                result = response.status, dict(response.getheaders()), response.read()
                conn.close()
                return result
            for _ in range(40):
                try:
                    get('/pypi/simple/')
                    break
                except (OSError, http.client.HTTPException):
                    time.sleep(0.25)
            else:
                raise AssertionError('Nginx did not start')
            for path, accept, mime, body in [
                ('/pypi/simple/', 'text/html', 'text/html', b'root HTML'),
                ('/pypi/simple/', 'application/vnd.pypi.simple.v1+json', 'application/vnd.pypi.simple.v1+json', b'projects'),
                ('/pypi/simple/foo-bar/', 'application/vnd.pypi.simple.v1+html', 'application/vnd.pypi.simple.v1+html', b'vendor project'),
                ('/pypi/simple/index.v1_json', '', 'application/vnd.pypi.simple.v1+json', b'projects'),
                ('/pypi/json/foo-bar', '', 'application/json', b'foo-bar'),
            ]:
                status, headers, content = get(path, {'Accept': accept})
                assert status == 200, (path, status, content)
                assert headers['Content-Type'] == mime, headers
                assert body in content, content
                if '/simple/' in path:
                    assert headers['Vary'] == 'Accept', headers
            for path, target in [
                ('/pypi/simple/Foo...__Bar', '/pypi/simple/foo-bar/'),
                ('/pypi/Foo_Bar/json', '/pypi/json/foo-bar'),
                ('/pypi/web/simple/', '/pypi/simple/'),
            ]:
                status, headers, _ = get(path)
                assert status == 301 and headers['Location'].endswith(target), (status, headers)
            blob = '/pypi/packages/ab/cd/1234/example.whl'
            status, headers, content = get(blob, {'Range': 'bytes=2-4'})
            assert status == 206 and content == b'234', (status, content)
            status, headers, content = get(blob, method='HEAD')
            assert status == 200 and not content and headers['Content-Length'] == '20'
            missing = blob.replace('example.whl', 'missing.whl')
            status, headers, _ = get(missing)
            assert status == 302 and headers['Location'] == 'https://mirrors.tuna.tsinghua.edu.cn/pypi/web/packages/ab/cd/1234/missing.whl'
            for path in ['/pypi/packages/ab/cd/1234/example.whl.tmp', '/pypi/local.db', '/pypi/.yukina/private', '/pypi/simple/missing/', '/pypi/json/missing']:
                status, _, content = get(path)
                assert status == 404 and b'DO NOT EXPOSE' not in content, (path, status)
            assert get('/pypi/simple/', {'Accept': 'image/png'})[0] == 406
            assert get(blob, {'X-Test-Forbidden': '1'})[0] == 403
            rows = [json.loads(line) for line in (logs / 'pypi.log').read_text().splitlines()]
            assert any(row['url'] == missing and row['proxied'] == '1' and row['status'] == 302 for row in rows)
            assert any(row['url'] == blob and row['proxied'] == '0' and row['status'] == 206 for row in rows)
            subprocess.run(['python3', str(HERE / 'verify_nginx.py'), '--base-url', f'http://127.0.0.1:{port}', '--host', 'localhost', '--storage', str(data / 'pypi')], check=True)
            print('Nginx/njs: negotiation, normalization, JSON, HEAD, Range, miss, privacy, policy and logs passed')
        except BaseException:
            subprocess.run(['docker', 'logs', name], check=False)
            raise
        finally:
            subprocess.run(['docker', 'stop', '-t', '1', name], check=False, stdout=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
