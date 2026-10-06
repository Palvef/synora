import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('terra', Path(__file__).resolve().parents[1] / 'terra.py')
terra = importlib.util.module_from_spec(spec)
spec.loader.exec_module(terra)


def listing(prefixes, truncated=False, marker=''):
    response = Mock()
    response.content = ('<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
                        + ''.join(f'<CommonPrefixes><Prefix>{p}</Prefix></CommonPrefixes>' for p in prefixes)
                        + f'<IsTruncated>{str(truncated).lower()}</IsTruncated><NextMarker>{marker}</NextMarker>'
                        + '</ListBucketResult>').encode()
    return response


class TerraTests(unittest.TestCase):
    def test_paginated_fedora_scope(self):
        pages = [listing(['terra44/', 'terra44-source/', 'um44/', 'terrael10/'], True, 'terra44/'),
                 listing(['terrarawhide/', 'terra45-mesa/', 'terra45-nvidia-source/', 'terra45/../../'])]
        with patch.object(terra.requests, 'get', side_effect=pages) as get:
            self.assertEqual(terra.discover('https://repo.test'), [
                ('terra44', ('44', 'main')), ('terra45-mesa', ('45', 'mesa')),
                ('terrarawhide', ('rawhide', 'main'))])
            self.assertEqual(get.call_args.kwargs['params']['marker'], 'terra44/')

    def test_empty_and_broken_pagination_fail_closed(self):
        for response in [listing([]), listing(['terra44/'], True)]:
            with patch.object(terra.requests, 'get', return_value=response), self.assertRaises(ValueError):
                terra.discover('https://repo.test')

    def test_repeated_marker_is_rejected(self):
        with patch.object(terra.requests, 'get', return_value=listing(['terra44/'], True, 'same')):
            with self.assertRaises(ValueError):
                terra.discover('https://repo.test')

    def test_sync_preserves_upstream_files_and_uses_tls_proxy_adapter(self):
        import os
        import tempfile
        with tempfile.TemporaryDirectory() as storage, \
                patch.dict(os.environ, {'SYNORA_STORAGE': storage, 'HTTPS_PROXY': 'http://proxy.test:14000'}), \
                patch.object(terra.sys, 'argv', ['terra.py']), \
                patch.object(terra, 'discover', return_value=[('terra44', ('44', 'main'))]), \
                patch.object(terra, 'Selection') as selection, \
                patch.object(terra.subprocess, 'run', return_value=Mock(returncode=0)) as run:
            selection.return_value.matches.return_value = True
            terra.main()
            args = run.call_args.args[0]
            self.assertEqual(args[0], 'rsync-ssl')
            self.assertIn('rsync://repos.fyralabs.com/repo/terra44/', args)
            self.assertIn('--delete-delay', args)
            self.assertFalse(any('exclude' in arg or 'filter' in arg for arg in args))
            self.assertEqual(run.call_args.kwargs['env']['RSYNC_SSL_TYPE'], 'openssl')
            self.assertTrue(run.call_args.kwargs['env']['RSYNC_SSL_OPENSSL'].endswith('rsync_ssl_proxy.py'))
