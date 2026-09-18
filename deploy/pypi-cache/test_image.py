#!/usr/bin/env python3
"""Exercise the built image with an isolated, tiny HTTP upstream (no production data)."""
import argparse
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='synora-pypi-image-test-') as temporary:
        root = Path(temporary)
        upstream = root / 'upstream'
        upstream.mkdir()
        blob = b'PyPI fixture package\n' * 128
        digest = hashlib.sha256(blob).hexdigest()
        relative = 'aa/bb/' + 'c' * 60 + '/fixture-1.0.tar.gz'
        package = upstream / 'packages' / relative
        package.parent.mkdir(parents=True)
        package.write_bytes(blob)
        class Handler(SimpleHTTPRequestHandler):
            mismatch = False
            def __init__(self, *a, **kw):
                super().__init__(*a, directory=str(upstream), **kw)
            def log_message(self, *a):
                pass
            def do_HEAD(self):
                if self.mismatch and self.path == '/packages/' + relative:
                    self.send_response(200)
                    self.send_header('Content-Length', str(len(blob)-1))
                    self.end_headers()
                else:
                    super().do_HEAD()
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        url = f'http://127.0.0.1:{server.server_port}/'
        metadata = {'info': {'name': 'fixture', 'version': '1.0', 'requires_python': None},
                    'last_serial': 1, 'releases': {'1.0': [{
                        'filename': package.name, 'url': url + 'packages/' + relative,
                        'digests': {'sha256': digest}, 'size': len(blob),
                        'packagetype': 'sdist', 'python_version': 'source',
                        'requires_python': None, 'yanked': False,
                        'upload_time_iso_8601': '2026-09-01T00:00:00Z',
                    }]}}
        (upstream / 'json').mkdir()
        (upstream / 'json/fixture').write_text(json.dumps(metadata))
        simple = upstream / 'simple/fixture'
        simple.mkdir(parents=True)
        (simple / 'index.v1_json').write_text(json.dumps({'meta': {'api-version': '1.0'},
            'name': 'fixture', 'files': [{'filename': package.name, 'url': url+'packages/'+relative,
                'hashes': {'sha256': digest}, 'size': len(blob)}]}))
        (upstream / 'local.json').write_text('{"fixture": 1}')
        (upstream / 'last_serial').write_text('1')
        logs = root / 'logs'
        logs.mkdir()
        rows = [dict(timestamp=time.time(), clientip=ip, url='/pypi/packages/'+relative,
                     size=len(blob), status=302, user_agent='pip/25.0', proxied='1')
                for ip in ['192.0.2.1','198.51.100.1','203.0.113.1']]
        (logs / 'pypi.log').write_text(''.join(json.dumps(row)+'\n' for row in rows))
        data = root / 'data'
        data.mkdir()
        def cycle(expected):
            command = ['docker','run','--rm','--network','host','--cap-drop=ALL',
                       '--security-opt=no-new-privileges','-v',f'{data}:/data',
                       '-v',f'{logs}:/nginx-log:ro','-e','PYPI_CACHE_BYTES=16384',
                       '-e','SHADOWMIRE_WORKERS=2','-e','PYPI_UPSTREAM='+url,args.image]
            result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                    text=True, timeout=180)
            print(result.stdout[-12000:])
            assert (result.returncode == 0) == expected, result.returncode
            return result
        try:
            cycle(True)
            assert (data / '.synora/initial-success.json').exists()
            assert (data / 'simple/fixture/index.v1_json').is_file()
            assert (data / 'packages' / relative).read_bytes() == blob
            missing = relative.replace('fixture-1.0.tar.gz','missing-1.0.tar.gz')
            for row in list(rows):
                rows.append(dict(row,url='/pypi/packages/'+missing))
            (logs / 'pypi.log').write_text(''.join(json.dumps(row)+'\n' for row in rows))
            result = cycle(True)
            assert 'SYNORA_STATUS=success_with_warnings' in result.stdout
            warnings = json.loads((data / '.synora/cache-warnings.json').read_text())
            assert warnings == {'count': 1, 'paths': [missing]}, warnings
            marker = (data / '.synora/last-success.json').read_bytes()
            (logs / 'pypi.log').write_text('malformed\n')
            cycle(False)
            assert (data / '.synora/last-success.json').read_bytes() == marker
            assert (data / 'packages' / relative).read_bytes() == blob
            (logs / 'pypi.log').write_text(''.join(json.dumps(row)+'\n' for row in rows))
            (upstream / 'local.json').write_text('{"fixture": 2}')
            (simple / 'index.v1_json').unlink()
            cycle(False)
            assert (data / '.synora/last-success.json').read_bytes() == marker
            # A lying HEAD response must not publish a file larger than budgeted.
            (upstream / 'local.json').write_text('{"fixture": 1}')
            (data / 'packages' / relative).unlink()
            for db in (data / '.synora').glob('*-size.db*'):
                db.unlink()
            Handler.mismatch = True
            cycle(False)
            assert not (data / 'packages' / relative).exists()
            assert (data / '.synora/last-success.json').read_bytes() == marker
            print('PASS: real image index/cache, missing-package warnings, invalid logs, critical metadata and size mismatch failures')
        finally:
            server.shutdown()


if __name__ == '__main__':
    main()
