import sys
from pathlib import Path
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'helpers'))
from package_policy import excluded, excluded_file

class PackagePolicyTests(unittest.TestCase):
    def test_debug_and_test_packages(self):
        for name in ['libc6-dbg', 'libfoo-dbgsym', 'kernel-debuginfo-common', 'kernel-debugsource', 'python3-tests', 'test-tools']:
            self.assertTrue(excluded(name), name)
        for name in ['libc6', 'libfoo-devel', 'python3-dev', 'contest', 'latest', 'libtestkit']:
            self.assertFalse(excluded(name), name)
    def test_file_names_do_not_match_versions_or_metadata(self):
        self.assertTrue(excluded_file('pool/libfoo-dbgsym_1.0_amd64.deb'))
        self.assertTrue(excluded_file('pool/libfoo_1.0_amd64.ddeb'))
        self.assertTrue(excluded_file('kernel-debuginfo-6.1-2.x86_64.rpm'))
        self.assertFalse(excluded_file('libfoo_1.0-test_amd64.deb'))
        self.assertFalse(excluded_file('testing/Packages.gz'))

    def test_rsync_removes_existing_excluded_packages(self):
        import shutil, subprocess, tempfile
        if not shutil.which('rsync'):
            self.skipTest('rsync is not installed')
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);src=root/'src';dst=root/'dst';src.mkdir();dst.mkdir()
            for name in ['libc6_1_amd64.deb','libc6-dbg_1_amd64.deb','kernel-debuginfo-6.1-2.x86_64.rpm','contest_1_amd64.deb']:
                (src/name).write_text('new');(dst/name).write_text('old')
            rules=Path(__file__).resolve().parents[1]/'helpers/package-filters.rules'
            subprocess.run(['rsync','-a','--delete','--filter','merge '+str(rules),str(src)+'/',str(dst)+'/'],check=True)
            self.assertEqual(sorted(p.name for p in dst.iterdir()),['contest_1_amd64.deb','libc6_1_amd64.deb'])
