import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from test_repo_sync import module


class AptRequestPacingTests(unittest.TestCase):
    def test_waits_between_network_requests_but_not_cache_hits(self):
        apt = module('apt-sync')
        response = Mock(headers={})
        response.iter_content.return_value = [b'index']
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {'APT_REQUEST_INTERVAL': '1'}), \
                patch.object(apt.requests, 'get', return_value=response) as get, \
                patch.object(apt.time, 'monotonic', side_effect=[10, 10, 10.25, 11]), \
                patch.object(apt.time, 'sleep') as sleep:
            path=Path(directory)/'index'
            self.assertEqual(apt.check_and_download('https://repo.test/one',path,True),0)
            self.assertEqual(apt.check_and_download('https://repo.test/one',path,True),0)
            self.assertEqual(apt.check_and_download('https://repo.test/two',path),0)
            self.assertEqual(get.call_count,2)
            sleep.assert_called_once_with(0.75)

    def test_invalid_intervals_fail_before_network_access(self):
        for interval in ['nan','inf','-1','61','invalid']:
            apt=module('apt-sync')
            with tempfile.TemporaryDirectory() as directory, \
                    patch.dict(os.environ,{'APT_REQUEST_INTERVAL':interval}), \
                    patch.object(apt.requests,'get') as get:
                self.assertEqual(apt.check_and_download('https://repo.test',Path(directory)/'index'),1)
                get.assert_not_called()
