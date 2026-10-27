import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'pypi-runtime'))
import clickhouse_logs as ck

NOW = 1793066400
ITEM = dict(timestamp=NOW-20, clientip='192.0.2.1', url='/pypi/packages/aa/bb/hash/file.whl',
            size='23', status=200, user_agent='pip/25', proxied='1', total_requests='1')
CONFIG = dict(endpoint='http://clickhouse.test:8123', username='pypi', password='secret')


class Response(io.BytesIO):
    def __enter__(self): return self
    def __exit__(self, *args): self.close()


class ClickHouseLogsTest(unittest.TestCase):
    def prepare(self, root, records, **kwargs):
        response = Response(b''.join(json.dumps(item).encode()+b'\n' for item in records))
        with patch.object(ck, 'open_query', return_value=response):
            return ck.prepare(root/'pypi.log', config=CONFIG, now=NOW, **kwargs)

    def test_exports_normalized_records_atomically(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            self.assertEqual(self.prepare(root,[ITEM]),1)
            item=json.loads((root/'pypi.log').read_text())
            self.assertNotIn('total_requests',item)
            self.assertEqual(item['size'],23)
            self.assertEqual(item['proxied'],'1')

    def test_empty_stale_invalid_or_truncated_results_preserve_previous_votes(self):
        for records in [[], [dict(ITEM,timestamp=NOW-7200)],
                        [dict(ITEM,status=0)], [dict(ITEM,total_requests='2')],
                        [dict(ITEM,url='/pypi/%2e%2e/secret')]]:
            with self.subTest(records=records), tempfile.TemporaryDirectory() as directory:
                root=Path(directory);out=root/'pypi.log';out.write_text('previous')
                with self.assertRaises(ValueError): self.prepare(root,records)
                self.assertEqual(out.read_text(),'previous')
                self.assertFalse(out.with_suffix('.tmp').exists())

    def test_error_after_valid_record_does_not_publish_partial_result(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);out=root/'pypi.log';out.write_text('previous')
            response=Response(json.dumps(ITEM).encode()+b'\nCode: 241 DB::Exception\n')
            with patch.object(ck,'open_query',return_value=response), self.assertRaises(ValueError):
                ck.prepare(out,config=CONFIG,now=NOW)
            self.assertEqual(out.read_text(),'previous')

    def test_unsafe_paths_are_skipped_and_counted_without_weakening_validation(self):
        records=[dict(ITEM,total_requests=2),dict(ITEM,total_requests=2,url='/pypi/packages/%2e%2e/secret')]
        with tempfile.TemporaryDirectory() as directory:
            messages=[]
            self.assertEqual(self.prepare(Path(directory),records,progress=messages.append),1)
            self.assertTrue(any('1 unsafe' in message for message in messages))

    def test_query_has_bounded_window_and_no_local_log_fallback(self):
        query=ck.query(CONFIG,NOW)
        self.assertIn('count() OVER ()',query)
        self.assertIn(str(NOW-7*86400),query)
        self.assertIn('/pypi/web/packages/',query)
        self.assertIn('FORMAT JSONEachRow',query)
        with self.assertRaises(ValueError): ck.query(dict(CONFIG,table='access_log; DROP TABLE x'),NOW)

    def test_query_failure_hides_credentials_and_preserves_votes(self):
        with tempfile.TemporaryDirectory() as directory:
            out=Path(directory)/'pypi.log';out.write_text('previous')
            with patch.object(ck,'open_query',side_effect=OSError('secret')),self.assertRaises(RuntimeError) as error:
                ck.prepare(out,config=CONFIG,now=NOW)
            self.assertNotIn('secret',str(error.exception))
            self.assertEqual(out.read_text(),'previous')

    def test_http_query_uses_header_auth_and_bypasses_sync_proxy(self):
        from unittest.mock import Mock
        opener=Mock()
        with patch.object(ck.urllib.request,'build_opener',return_value=opener):
            ck.open_query(CONFIG,'SELECT 1')
        args,kwargs=opener.open.call_args
        request=args[0]
        self.assertEqual(request.get_method(),'POST')
        self.assertNotIn('secret',request.full_url)
        self.assertTrue(request.get_header('Authorization').startswith('Basic '))
        self.assertEqual(request.data,b'SELECT 1')
        self.assertEqual(kwargs['timeout'],120)

    def test_no_configuration_never_falls_back_to_disk(self):
        import os
        with patch.dict(os.environ,{},clear=True),self.assertRaisesRegex(ValueError,'local access logs are not used'):
            ck.configuration()
