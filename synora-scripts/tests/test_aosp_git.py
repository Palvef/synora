import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

HELPER = Path(__file__).resolve().parents[1] / 'helpers' / 'aosp_git.py'


class AospMirrorTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'source'
        self.mirror = self.root / 'mirror'
        self.git('init', str(self.source))
        self.git('-C', str(self.source), '-c', 'user.name=Test', '-c', 'user.email=test@example.org', 'commit', '--allow-empty', '-m', 'initial')
        self.git('-C', str(self.source), '-c', 'user.name=Test', '-c', 'user.email=test@example.org', 'tag', '-a', 'signed-release', '-m', 'release')
        self.git('-C', str(self.source), 'symbolic-ref', 'refs/heads/security-release', 'refs/tags/signed-release')
        self.git('init', '--bare', str(self.mirror))
        self.git('-C', str(self.mirror), 'remote', 'add', 'aosp', str(self.source))

    def git(self, *args):
        return subprocess.run(['git', *args], check=True, capture_output=True, text=True).stdout.strip()

    def fetch(self):
        command = ['python3', str(HELPER)] if HELPER.exists() else ['git']
        return subprocess.run(command + ['fetch', '--prune', 'aosp', '+refs/heads/*:refs/heads/*', '+refs/tags/*:refs/tags/*'], cwd=self.mirror, env={**os.environ, 'SYNORA_AOSP_REAL_GIT': shutil.which('git')}, capture_output=True, text=True)

    def test_non_commit_branch_preserves_upstream_object_and_prunes(self):
        result = self.fetch()
        self.assertEqual(result.returncode, 0, result.stderr)
        oid = self.git('-C', str(self.source), 'rev-parse', 'signed-release')
        self.assertEqual(self.git('-C', str(self.mirror), 'rev-parse', 'refs/heads/security-release'), oid)
        advertised = self.git('ls-remote', str(self.mirror))
        self.assertIn(oid + '\trefs/heads/security-release', advertised)
        self.assertNotIn('refs/synora/', advertised)
        self.git('-C', str(self.source), 'symbolic-ref', '--delete', 'refs/heads/security-release')
        self.assertEqual(self.fetch().returncode, 0)
        self.assertNotIn('security-release', self.git('ls-remote', str(self.mirror)))

    def test_fetch_failure_preserves_existing_branches(self):
        self.assertEqual(self.fetch().returncode, 0)
        before = self.git('-C', str(self.mirror), 'show-ref')
        self.git('-C', str(self.mirror), 'remote', 'set-url', 'aosp', str(self.root / 'missing'))
        self.assertNotEqual(self.fetch().returncode, 0)
        self.assertEqual(self.git('-C', str(self.mirror), 'show-ref'), before)

    def test_symbolic_branch_can_become_normal_commit_branch(self):
        self.assertEqual(self.fetch().returncode, 0)
        self.git('-C', str(self.source), 'symbolic-ref', '--delete', 'refs/heads/security-release')
        self.git('-C', str(self.source), 'branch', 'security-release')
        self.assertEqual(self.fetch().returncode, 0)
        self.assertEqual(self.git('-C', str(self.mirror), 'rev-parse', 'refs/heads/security-release'), self.git('-C', str(self.source), 'rev-parse', 'HEAD'))
