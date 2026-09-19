import http.server
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'pushsync.sh'

class RestartTest(unittest.TestCase):
    def exercise(self, failure=False, lost=False):
        calls = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_POST(self):
                calls.append(self.path.rsplit('/', 1)[-1])
                self.send_response(403 if failure else 200)
                self.end_headers()
                self.wfile.write(b'"new-run"')
            def do_GET(self):
                calls.append('history')
                status = 'lost' if lost else ('cancelling' if calls.count('history') == 1 else 'cancelled')
                self.send_response(200)
                self.end_headers()
                self.wfile.write(('[{"status":"' + status + '"}]').encode())
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as temp:
                mapping = Path(temp) / 'map'
                mapping.write_text('kali kali web\n')
                env = dict(os.environ, NO_PROXY='127.0.0.1', no_proxy='127.0.0.1', KEY_ID='kali', KEY_REPO_MAP=str(mapping),
                           SYNORA_ENV_FILE=temp+'/absent', SYNORA_API=f'http://127.0.0.1:{server.server_port}',
                           SYNORA_TOKEN='test', SYNORA_PUSH_LOCK_DIR=temp+'/locks', SYNORA_PUSH_POLL_INTERVAL='0.01')
                result = subprocess.run(['bash', str(SCRIPT), '-c', 'sync:archive:kali'], env=env, capture_output=True, timeout=10)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        return result, calls

    def test_waits_for_cancellation_before_restart(self):
        result, calls = self.exercise()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, ['stop', 'history', 'stop', 'history', 'run'])
    def test_stop_failure_does_not_restart(self):
        result, calls = self.exercise(failure=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('run', calls)
    def test_lost_writer_does_not_restart(self):
        result, calls = self.exercise(lost=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('run', calls)

if __name__ == '__main__':
    unittest.main()
