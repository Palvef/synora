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
