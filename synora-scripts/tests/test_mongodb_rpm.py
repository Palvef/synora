import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'helpers'))
import mongodb_rpm as mongo

class MongoRpmTests(unittest.TestCase):
    def test_only_missing_primary_can_trigger_repair(self):
        index=Mock(content=b'<repomd><data type="primary"><location href="repodata/primary.xml.gz"/></data></repomd>')
        missing=Mock(status_code=404)
        with patch.object(mongo.requests,'get',side_effect=[index,missing]):
            self.assertTrue(mongo.needs_repair('https://repo.mongodb.org/yum/redhat/5/mongodb-org/3.1/x86_64'))
        error=Mock(status_code=503);error.raise_for_status.side_effect=RuntimeError('unavailable')
        with patch.object(mongo.requests,'get',side_effect=[index,error]),self.assertRaises(RuntimeError):
            mongo.needs_repair('https://repo.mongodb.org/yum/redhat/5/mongodb-org/3.1/x86_64')
        self.assertFalse(mongo.needs_repair('https://other.example.test/repo'))

    def test_listing_must_stay_in_repository(self):
        response=Mock(content=b'<ListBucketResult><Contents><Key>yum/elsewhere/a.rpm</Key><Size>1</Size><ETag>"aa"</ETag></Contents><IsTruncated>false</IsTruncated></ListBucketResult>')
        with patch.object(mongo.requests,'get',return_value=response),self.assertRaises(RuntimeError):
            mongo.inventory('https://repo.mongodb.org/yum/redhat/5/mongodb-org/3.1/x86_64')

    def test_bad_checksum_preserves_existing_file(self):
        with tempfile.TemporaryDirectory() as d:
            path=Path(d)/'a.rpm';path.write_bytes(b'old')
            response=Mock();response.iter_content.return_value=[b'bad'];response.__enter__=Mock(return_value=response);response.__exit__=Mock(return_value=False)
            with patch.object(mongo.requests,'get',return_value=response),self.assertRaises(RuntimeError):
                mongo.download('https://example.test/a.rpm',path,3,'0'*32)
            self.assertEqual(path.read_bytes(),b'old')

    def test_recovery_prunes_only_after_successful_inventory_download(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            old=root/'old-1-1.x86_64.rpm';old.write_bytes(b'old')
            def download(url,path,size,etag): path.write_bytes(b'new')
            with patch.object(mongo,'inventory',return_value={'new-1-1.x86_64.rpm':(3,'')}),patch.object(mongo,'download',side_effect=download):
                mongo.recover('https://repo.mongodb.org/example',root)
            self.assertFalse(old.exists())
            self.assertTrue((root/'new-1-1.x86_64.rpm').exists())
            old.write_bytes(b'old')
            with patch.object(mongo,'inventory',return_value={'new-1-1.x86_64.rpm':(3,'')}),patch.object(mongo,'download',side_effect=RuntimeError('transfer failed')),self.assertRaises(RuntimeError):
                mongo.recover('https://repo.mongodb.org/example',root)
            self.assertTrue(old.exists())
