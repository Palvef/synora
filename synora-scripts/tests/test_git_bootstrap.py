import functools
import io
import http.server
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'helpers'))
import git_bootstrap


class BootstrapTests(unittest.TestCase):
    def test_seed_objects_then_fetch_official_refs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, mirror = root / 'source', root / 'mirror.git'
            def git(*args):
                return subprocess.check_output(['git', *map(str, args)], stderr=subprocess.DEVNULL, text=True).strip()
            git('init', source)
            git('-C', source, '-c', 'user.name=Test', '-c', 'user.email=test@example.com', 'commit', '--allow-empty', '-m', 'seed')
            git('-C', source, 'gc')
            old_packs = set((source / '.git/objects/pack').glob('*.pack'))
            git('-C', source, '-c', 'user.name=Test', '-c', 'user.email=test@example.com', 'commit', '--allow-empty', '-m', 'newer')
            git('-C', source, 'repack', '-d')
            git('-C', source, 'update-server-info')
            new_packs = set((source / '.git/objects/pack').glob('*.pack')) - old_packs
            (source / '.git/objects/info/packs').write_text(''.join('P ' + p.name + '\n' for p in [*new_packs, *old_packs]))
            git('init', '--bare', mirror)
            server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(source / '.git')))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                git_bootstrap.bootstrap(f'http://127.0.0.1:{server.server_port}', mirror)
                self.assertEqual(git('--git-dir', mirror, 'rev-parse', 'refs/synora-bootstrap/seed'), git('-C', source, 'rev-parse', 'HEAD'))
                git('--git-dir', mirror, 'remote', 'add', '--mirror=fetch', 'origin', source)
                git('--git-dir', mirror, 'remote', 'update', '--prune')
                self.assertEqual(git('--git-dir', mirror, 'show-ref'), git('-C', source, 'show-ref'))
                git('--git-dir', mirror, 'fsck', '--full')
            finally:
                server.shutdown()
                server.server_close()
                thread.join()

    def test_download_resumes_or_restarts_without_appending_full_response(self):
        class Response(io.BytesIO):
            def __init__(self, body, status, headers):
                super().__init__(body)
                self.status, self.headers = status, headers
        class Opener:
            def __init__(self, response):
                self.response = response
            def open(self, request, timeout):
                self.request = request
                return self.response
        for status, body, headers in [(206, b'def', {'Content-Length': '3', 'Content-Range': 'bytes 3-5/6'}), (200, b'abcdef', {'Content-Length': '6'})]:
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'pack'
                path.write_bytes(b'abc')
                opener = Opener(Response(body, status, headers))
                git_bootstrap.download(opener, 'http://example.test/pack', path)
                self.assertEqual(path.read_bytes(), b'abcdef')
                self.assertEqual(opener.request.get_header('Range'), 'bytes=3-')

    def test_corrupt_pack_never_publishes_seed_ref(self):
        class Opener:
            def open(self, url, timeout):
                values = {'HEAD': b'ref: refs/heads/master', 'info/refs': b'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/master', 'objects/info/packs': b'P pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack'}
                return io.BytesIO(values[url.removeprefix('http://example.test/')])
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            subprocess.run(['git', 'init', '--bare', str(repo)], check=True, capture_output=True)
            with patch.object(git_bootstrap.urllib.request, 'build_opener', return_value=Opener()), patch.object(git_bootstrap, 'download', side_effect=lambda opener, url, path: path.write_bytes(b'corrupt')):
                with self.assertRaises(subprocess.CalledProcessError):
                    git_bootstrap.bootstrap('http://example.test', repo)
            result = subprocess.run(['git', '-C', str(repo), 'show-ref'], capture_output=True)
            self.assertEqual(result.returncode, 1)

    def test_large_ref_advertisement_is_scanned_without_whole_file_limit(self):
        refs = (b'b' * 40 + b' refs/heads/feature\n') * 300000
        refs += b'a' * 40 + b' refs/heads/master\n'
        self.assertGreater(len(refs), 16 * 1024 * 1024)
        self.assertEqual(git_bootstrap.lookup_head(io.BytesIO(refs), 'refs/heads/master'), 'a' * 40)

    def test_invalid_pack_names_are_rejected(self):
        for text in ['P ../../config', 'P pack-123.pack', '<html>error</html>', '']:
            with self.assertRaises(ValueError):
                git_bootstrap.pack_names(text)
