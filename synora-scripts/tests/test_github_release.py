import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'github-release.py'
spec = importlib.util.spec_from_file_location('github_release', SCRIPT)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
PAYLOAD = b'github-release-fixture\n' * 100000
CUT = 1 << 20


class Downloads(unittest.TestCase):
    def setUp(self):
        env = patch.dict(os.environ, {name: "" for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]} | {"NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost"})
        env.start()
        os.environ.pop("GITHUB_TOKEN", None)
        self.addCleanup(env.stop)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dest = Path(self.temp.name) / 'asset.tar.xz'
        self.dest.write_bytes(b'previous valid release')
        self.requests = []
        self.break_first = True
        self.ignore_range = False
        self.bad_range = False
        self.status = 200
        parent = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_GET(self):
                parent.requests.append(dict(self.headers))
                if parent.status != 200:
                    self.send_error(parent.status)
                    return
                offset = int(self.headers.get('Range', 'bytes=0-')[6:-1])
                if parent.ignore_range:
                    offset = 0
                self.send_response(206 if offset else 200)
                self.send_header('Content-Length', str(len(PAYLOAD)-offset))
                self.send_header('ETag', '"fixture-v1"')
                if offset:
                    start = offset+1 if parent.bad_range else offset
                    self.send_header('Content-Range', f'bytes {start}-{len(PAYLOAD)-1}/{len(PAYLOAD)}')
                self.end_headers()
                if parent.break_first and len(parent.requests) == 1:
                    self.wfile.write(PAYLOAD[:CUT])
                    self.wfile.flush()
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                else:
                    try:
                        self.wfile.write(PAYLOAD[offset:])
                    except (BrokenPipeError, ConnectionResetError):
                        pass
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.url = f'http://127.0.0.1:{self.server.server_port}/asset'

    def test_broken_stream_retries_remaining_bytes(self):
        with patch.object(release, 'RETRY_DELAY', 0, create=True):
            release.do_download(self.url, self.dest, 1000, len(PAYLOAD))
        self.assertEqual(self.dest.read_bytes(), PAYLOAD)
        self.assertEqual(self.requests[1].get('Range'), f'bytes={CUT}-')
        self.assertEqual(self.requests[1].get('If-Range'), '"fixture-v1"')
        self.assertEqual(int(self.dest.stat().st_mtime), 1000)

    def test_interrupted_run_preserves_old_file_and_resumes_next_run(self):
        with patch.object(release, 'DOWNLOAD_ATTEMPTS', 1, create=True):
            with self.assertRaises(Exception):
                release.do_download(self.url, self.dest, 1000, len(PAYLOAD))
        self.assertEqual(self.dest.read_bytes(), b'previous valid release')
        release.do_download(self.url, self.dest, 1000, len(PAYLOAD))
        self.assertEqual(self.dest.read_bytes(), PAYLOAD)
        self.assertEqual(self.requests[1].get('Range'), f'bytes={CUT}-')

    def test_range_ignored_restarts_instead_of_appending(self):
        self.ignore_range = True
        with patch.object(release, 'RETRY_DELAY', 0, create=True):
            release.do_download(self.url, self.dest, 1000, len(PAYLOAD))
        self.assertEqual(self.dest.read_bytes(), PAYLOAD)

    def test_bad_content_range_cannot_replace_valid_file(self):
        self.bad_range = True
        with patch.object(release, 'DOWNLOAD_ATTEMPTS', 2, create=True), patch.object(release, 'RETRY_DELAY', 0, create=True):
            with self.assertRaises(Exception):
                release.do_download(self.url, self.dest, 1000, len(PAYLOAD))
        self.assertEqual(self.dest.read_bytes(), b'previous valid release')
        self.assertEqual(len(self.requests), 2)

    def test_permanent_http_error_is_not_retried(self):
        self.status = 404
        with patch.object(release, 'RETRY_DELAY', 0, create=True):
            with self.assertRaises(Exception):
                release.do_download(self.url, self.dest, 1000, len(PAYLOAD))
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.dest.read_bytes(), b'previous valid release')

    def test_digest_is_verified_before_atomic_publish(self):
        self.break_first = False
        release.do_download(self.url, self.dest, 1000, len(PAYLOAD), remote_digest='sha256:'+hashlib.sha256(PAYLOAD).hexdigest())
        self.assertEqual(self.dest.read_bytes(), PAYLOAD)
        self.dest.write_bytes(b'valid previous')
        with self.assertRaises(Exception):
            release.do_download(self.url, self.dest, 1001, len(PAYLOAD), remote_digest='sha256:'+'0'*64)
        self.assertEqual(self.dest.read_bytes(), b'valid previous')

    def test_single_stable_release_uses_official_latest(self):
        response = release.requests.Response()
        response.status_code = 200
        response._content = json.dumps({'tag_name': 'v3.15.0'}).encode()
        response._content_consumed = True
        function = getattr(release, 'release_generator', None)
        self.assertIsNotNone(function, 'Release selection must be independently testable')
        with patch.object(release, 'github_get', return_value=response) as request:
            selected = list(function('prometheus/prometheus', 'https://api.github.com/repos/', latest_only=True))
        self.assertEqual(selected, [{'tag_name': 'v3.15.0'}])
        self.assertEqual(request.call_args[0][0], 'https://api.github.com/repos/prometheus/prometheus/releases/latest')

    def test_existing_repositories_keep_only_one_release(self):
        cfg = json.loads(SCRIPT.with_suffix('.json').read_text())
        self.assertEqual(len(cfg), 73)
        for item in cfg:
            item = item if isinstance(item, dict) else {'repo': item}
            self.assertEqual(item.get('versions', 1), 1, item['repo'])
            self.assertNotIn('release_versions', item)
            self.assertNotIn('pre_release_versions', item)
        self.assertIn('prometheus/prometheus', cfg)



if __name__ == '__main__':
    unittest.main()
