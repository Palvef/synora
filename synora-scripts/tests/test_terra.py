import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, MagicMock, patch

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
        client = Mock()
        client.get.side_effect = pages
        with patch.object(terra, 'session', return_value=MagicMock(__enter__=Mock(return_value=client))):
            get = client.get
            self.assertEqual(terra.discover('https://repo.test'), [
                ('terra44', ('44', 'main')), ('terra45-mesa', ('45', 'mesa')),
                ('terrarawhide', ('rawhide', 'main'))])
            self.assertEqual(get.call_args.kwargs['params']['marker'], 'terra44/')

    def test_empty_and_broken_pagination_fail_closed(self):
        for response in [listing([]), listing(['terra44/'], True)]:
            with patch.object(terra, 'session', return_value=MagicMock(__enter__=Mock(return_value=Mock(get=Mock(return_value=response))))), self.assertRaises(ValueError):
                terra.discover('https://repo.test')

    def test_repeated_marker_is_rejected(self):
        with patch.object(terra, 'session', return_value=MagicMock(__enter__=Mock(return_value=Mock(get=Mock(return_value=listing(['terra44/'], True, 'same')))))):
            with self.assertRaises(ValueError):
                terra.discover('https://repo.test')

    def test_sync_passes_selected_repository_to_unfiltered_s3_mirror(self):
        import os
        import tempfile
        with tempfile.TemporaryDirectory() as storage, \
                patch.dict(os.environ, {'SYNORA_STORAGE': storage}), \
                patch.object(terra.sys, 'argv', ['terra.py']), \
                patch.object(terra, 'discover', return_value=[('terra44', ('44', 'main'))]), \
                patch.object(terra, 'Selection') as selection, \
                patch.object(terra, 'sync') as sync:
            selection.return_value.matches.return_value = True
            terra.main()
            self.assertEqual(sync.call_args.args[1:], ('terra44', Path(storage)))
