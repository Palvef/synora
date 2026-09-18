import json
import gzip
import fcntl
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import logs
import sync
import sys

NOW = 1789732800
ITEM = dict(timestamp=NOW, clientip='192.0.2.1', url='/pypi/web/packages/aa/bb/abcd/pkg.whl?x=1', size=23, status=200, user_agent='pip/25', proxied='0')

class LogsTest(unittest.TestCase):
    def test_root_without_slash(self):
        for uri in ['/pypi', '/pypi/web']:
            self.assertEqual(logs.normalize(dict(ITEM,url=uri))['url'],'/pypi/')
    def test_canonical(self):
        item = logs.normalize(dict(ITEM))
        self.assertEqual(item['url'], '/pypi/packages/aa/bb/abcd/pkg.whl')
        self.assertEqual(item['proxied'], '0')
    def test_traversal(self):
        for uri in ['/pypi/packages/%2e%2e/secret', '/pypi/packages/a/%252e%252e/secret']:
            with self.assertRaises(ValueError):
                logs.normalize(dict(ITEM, url=uri))
    def test_legacy_extra_column(self):
        item = logs.legacy('192.0.2.1 - - [18/Sep/2026:12:00:00 +0000] "GET /pypi/web/packages/aa/bb/hash/pkg.whl HTTP/1.1" 200 123 "application/octet-stream" "-" "pip/25" - https "mirror.nyist.edu.cn" - "0.1" "0.1" "-"')
        self.assertEqual(item['user_agent'], 'pip/25')
        self.assertEqual(item['proxied'], '1')
    def test_fail_closed_and_atomic(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); src=root/'source'; src.mkdir(); dest=root/'out'; dest.mkdir()
            out=dest/'pypi.log'; out.write_text('previous')
            with self.assertRaises(ValueError): logs.prepare(src, None, out, NOW)
            (src/'pypi.log').write_text(json.dumps(ITEM)+'\nnot-json\n')
            with self.assertRaises(ValueError): logs.prepare(src,None,out,NOW)
            self.assertEqual(out.read_text(),'previous')
    def test_two_sites_and_gzip(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); source=root/'new'; source.mkdir(); historic=root/'old'; historic.mkdir()
            (source/'pypi.log').write_text(json.dumps(ITEM)+'\n')
            site=historic/'ha'; site.mkdir()
            line='192.0.2.2 - - [18/Sep/2026:12:00:00 +0000] "GET /pypi/packages/aa/bb/hash/pkg.whl HTTP/1.1" 302 0 "text/html" "-" "pip/25" - https "mirrors.ha.edu.cn"'
            with gzip.open(site/'pypi.log.1.gz','wt') as stream: stream.write(line+'\n')
            count=logs.prepare(source,historic,root/'out.log',NOW)
            self.assertEqual(count,2)
    def test_expired_logs_do_not_authorize_gc(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); (root/'pypi.log').write_text(json.dumps(dict(ITEM,timestamp=NOW-8*86400))+'\n')
            with self.assertRaises(ValueError): logs.prepare(root,None,root/'out.log',NOW)

class SyncTest(unittest.TestCase):
    def test_process_collects_missing_paths(self):
        result=sync.run([sys.executable,'-c',"print('SYNORA_MISSING=aa/bb/hash/file.whl'); print('SYNORA_MISSING=aa/bb/hash/file.whl')"])
        self.assertEqual(result,['aa/bb/hash/file.whl'])
    def test_process_propagates_failure(self):
        with self.assertRaises(RuntimeError):
            sync.run([sys.executable,'-c','raise SystemExit(2)'])
    def test_budget_and_cli(self):
        cmd=sync.cache_command(Path('/data'),Path('/logs'),'https://upstream/web/',549755813888)
        self.assertEqual(cmd[cmd.index('--size-limit')+1], '549755813888')
        self.assertIn('--strip-prefix',cmd)
    def test_failed_index_stops_cache_and_marker(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)
            with patch.object(sync,'prepare',return_value=1), patch.object(sync,'run',side_effect=RuntimeError('failed')) as run:
                with self.assertRaises(RuntimeError): sync.cycle(root,root,None,'https://upstream/web/',512)
                self.assertEqual(run.call_count,1)
                self.assertFalse((root/'.synora/initial-success.json').exists())
    def test_failed_cache_stops_marker(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)
            with patch.object(sync,'prepare',return_value=1), patch.object(sync,'run',side_effect=[None,RuntimeError('cache failed')]):
                with self.assertRaises(RuntimeError): sync.cycle(root,root,None,'https://upstream/web/',512)
                self.assertFalse((root/'.synora/initial-success.json').exists())
    def test_concurrent_run_does_not_start(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); state=root/'.synora';state.mkdir()
            with (state/'sync.lock').open('a') as lock:
                fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                with patch.object(sync,'prepare') as prepare:
                    with self.assertRaises(BlockingIOError): sync.cycle(root,root,None,'https://upstream/web/',512)
                    prepare.assert_not_called()
    def test_empty_cache_not_bootstrapped(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)
            with patch.object(sync,'prepare',return_value=1), patch.object(sync,'run'):
                with self.assertRaises(RuntimeError): sync.cycle(root,root,None,'https://upstream/web/',512)
    def test_success_marker(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); p=root/'packages/aa/bb/hash/a.whl'; p.parent.mkdir(parents=True);p.write_bytes(b'a')
            with patch.object(sync,'prepare',return_value=1), patch.object(sync,'run'):
                sync.cycle(root,root,None,'https://upstream/web/',512)
                self.assertTrue((root/'.synora/initial-success.json').exists())

if __name__=='__main__': unittest.main()
