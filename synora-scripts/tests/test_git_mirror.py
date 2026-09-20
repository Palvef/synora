import os
from pathlib import Path
import subprocess
import tempfile
import unittest

HELPER = Path(__file__).resolve().parents[1] / "helpers/git_mirror.sh"


class GitMirrorTests(unittest.TestCase):
    def test_bootstrap_and_configured_deadlines(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "mirror.git"
            subprocess.run(["git", "init", "--bare", str(repo)], check=True, capture_output=True)
            def deadline():
                return subprocess.check_output(["bash", "-c", 'source "$1"; git_mirror_timeout "$2"', "bash", str(HELPER), str(repo)], text=True).strip()
            self.assertEqual(deadline(), "6h")
            subprocess.run(["git", "--git-dir", str(repo), "config", "synora.syncTimeout", "12h"], check=True)
            self.assertEqual(deadline(), "12h")
            subprocess.run(["git", "--git-dir", str(repo), "config", "synora.syncTimeout", "0"], check=True)
            with self.assertRaises(subprocess.CalledProcessError):
                deadline()

    def test_local_mirror_and_incremental_deadline(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "source"
            mirror = Path(directory) / "mirror.git"
            subprocess.run(["git", "init", str(source)], check=True, capture_output=True)
            env = dict(os.environ, GIT_AUTHOR_NAME="Test", GIT_AUTHOR_EMAIL="test@example.com", GIT_COMMITTER_NAME="Test", GIT_COMMITTER_EMAIL="test@example.com")
            subprocess.run(["git", "-C", str(source), "commit", "--allow-empty", "-m", "initial"], check=True, capture_output=True, env=env)
            result = subprocess.run(["bash", "-ec", 'source "$1"; git_mirror_init "$2" "$3"; git_mirror_update "$2" "$3"; git_mirror_timeout "$3"', "bash", str(HELPER), str(source), str(mirror)], check=True, text=True, capture_output=True)
            self.assertTrue(result.stdout.rstrip().endswith("1h"), result.stdout)
            subprocess.run(["git", "--git-dir", str(mirror), "fsck", "--full"], check=True, capture_output=True)

    def test_transfer_timeout_remains_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "mirror.git"
            subprocess.run(["git", "init", "--bare", str(repo)], check=True, capture_output=True)
            subprocess.run(["git", "--git-dir", str(repo), "remote", "add", "origin", "unused"], check=True)
            result = subprocess.run(["bash", "-ec", 'source "$1"; git_mirror_transfer() { return 124; }; git_mirror_update unused "$2"', "bash", str(HELPER), str(repo)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 124)
            self.assertIn("rc=124", result.stdout)
            self.assertNotIn("DONE", result.stdout)
