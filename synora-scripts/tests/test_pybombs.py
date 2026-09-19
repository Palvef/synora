import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "pybombs.sh"

class PybombsTests(unittest.TestCase):
    def run_wrapper(self, root, helper_body):
        helper = root / "helper"
        helper.mkdir()
        for name in ["upstream-recipe-repos.urls", "pre-replace-upstream.urls", "ignore.urls"]:
            (helper / name).touch()
        command = helper / "pybombs-mirror.sh"
        command.write_text("#!/bin/bash\nset -e\n" + helper_body)
        command.chmod(0o755)
        env = {**os.environ, "SYNORA_STORAGE": str(root / "data"), "PYBOMBS_MIRROR_SCRIPT_PATH": str(helper)}
        return subprocess.run(["bash", str(SCRIPT)], env=env, capture_output=True, text=True)

    def test_no_recipe_output_fails(self):
        with tempfile.TemporaryDirectory() as d:
            result = self.run_wrapper(Path(d), "exit 0\n")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("No usable PyBOMBS recipes", result.stderr)

    def test_partial_source_failure_keeps_usable_http_git_and_warns(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            result = self.run_wrapper(root, '''
mkdir -p "$SYNORA_STORAGE/recipes"
git init -q "$SYNORA_STORAGE/origin"
git -C "$SYNORA_STORAGE/origin" -c user.name=test -c user.email=test@example.test commit -q --allow-empty -m fixture
git clone -q --bare "$SYNORA_STORAGE/origin" "$SYNORA_STORAGE/recipes/test.git"
printf 'wget+https://example.test/missing.tar.gz\\n' > "$SYNORA_STORAGE/failed.log"
''')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("SYNORA_STATUS=success_with_warnings", result.stdout)
            self.assertIn("https://example.test/missing.tar.gz", result.stdout)
            self.assertIn("SYNORA_SIZE=", result.stdout)
            self.assertTrue((root / "data/recipes/test.git/info/refs").is_file())
