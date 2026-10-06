import importlib.util
from hashlib import md5
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('terra_s3', Path(__file__).resolve().parents[1] / 'helpers/terra_s3.py')
s3 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s3)


def info(content):
    return {'size': len(content), 'etag': md5(content, usedforsecurity=False).hexdigest()}


class S3Tests(unittest.TestCase):
    def test_proxy_required_and_no_proxy_cannot_bypass_it(self):
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError):
            s3.session()
        with patch.dict(os.environ, {'HTTPS_PROXY': 'http://proxy.test:14000', 'NO_PROXY': '*'}, clear=True):
            with s3.session() as client:
                self.assertFalse(client.trust_env)
                self.assertEqual(client.proxies['https'], 'http://proxy.test:14000')

    def test_no_metadata_publish_or_deletion_when_payload_fails(self):
        with tempfile.TemporaryDirectory() as root:
            target = Path(root) / 'terra44'
            (target / 'repodata').mkdir(parents=True)
            (target / 'repodata/repomd.xml').write_bytes(b'old')
            (target / 'obsolete.rpm').write_bytes(b'old')
            listing = {'package.rpm': info(b'pkg'), 'repodata/repomd.xml': info(b'new')}
            with patch.object(s3, 'objects', return_value=listing), patch.object(s3, 'download', side_effect=OSError('network')):
                with self.assertRaises(OSError):
                    s3.sync('https://upstream.test', 'terra44', Path(root))
            self.assertEqual((target / 'repodata/repomd.xml').read_bytes(), b'old')
            self.assertTrue((target / 'obsolete.rpm').exists())

    def test_original_metadata_and_debug_packages_are_preserved_and_removed_files_deleted(self):
        contents = {'debug-test.rpm': b'pkg', 'repodata/repomd.xml': b'signed xml', 'repodata/repomd.xml.asc': b'signature'}
        listing = {path: info(data) for path, data in contents.items()}
        def download(base, repository, path, metadata, target):
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(contents[path])
        with tempfile.TemporaryDirectory() as root:
            target = Path(root) / 'terra44'
            target.mkdir()
            (target / 'obsolete.rpm').write_bytes(b'old')
            with patch.object(s3, 'objects', return_value=listing), patch.object(s3, 'download', side_effect=download) as get:
                s3.sync('https://upstream.test', 'terra44', Path(root))
                self.assertEqual(get.call_args_list[0].args[2], 'debug-test.rpm')
                for path, data in contents.items():
                    self.assertEqual((target / path).read_bytes(), data)
                self.assertFalse((target / 'obsolete.rpm').exists())
                get.reset_mock()
                s3.sync('https://upstream.test', 'terra44', Path(root))
                get.assert_not_called()

    def test_size_or_etag_mismatch_does_not_replace_live_object(self):
        client = Mock()
        client.__enter__ = Mock(return_value=client)
        client.__exit__ = Mock(return_value=False)
        response = Mock()
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        response.iter_content.return_value = [b'bad']
        client.get.return_value = response
        with tempfile.TemporaryDirectory() as root, patch.object(s3, 'session', return_value=client):
            destination = Path(root) / 'repomd.xml'
            destination.write_bytes(b'old')
            with self.assertRaises(ValueError):
                s3.download('https://upstream.test', 'terra44', 'repomd.xml', info(b'new'), destination)
            self.assertEqual(destination.read_bytes(), b'old')
            self.assertEqual(list(Path(root).iterdir()), [destination])
