import sys,tempfile,unittest
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'helpers'))
from pybombs_cleanup import cleanup

class CleanupTests(unittest.TestCase):
    def test_complete_inventory_removes_only_unreferenced_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'git').mkdir()
            (root/'recipes-origin.urls').write_text('git+https://example.test/org/current.git/\n')
            (root/'git/org_current.git').mkdir();(root/'git/org_old.git').mkdir()
            self.assertEqual(cleanup(root),1)
            self.assertTrue((root/'git/org_current.git').exists())
    def test_failed_or_empty_inventory_cannot_delete(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'wget').mkdir();old=root/'wget/old';old.write_text('old')
            (root/'failed.log').write_text('failed URL')
            self.assertEqual(cleanup(root),0);self.assertTrue(old.exists())
            (root/'failed.log').unlink();(root/'recipes-origin.urls').write_text('')
            with self.assertRaises(RuntimeError):cleanup(root)
            self.assertTrue(old.exists())
