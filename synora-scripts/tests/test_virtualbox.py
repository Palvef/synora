import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / 'virtualbox.sh'


class VirtualBoxUpstreamTests(unittest.TestCase):
    def first_repository(self, upstream=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copy2(SCRIPT, root / SCRIPT.name)
            helper = root / 'yum-sync.py'
            helper.write_text('#!/bin/bash\nprintf "%s\\n" "$1"\nexit 42\n')
            helper.chmod(0o755)
            env = {'PATH': os.environ['PATH'], 'SYNORA_STORAGE': str(root / 'data')}
            if upstream is not None:
                env['SYNORA_UPSTREAM'] = upstream
            result = subprocess.run(['bash', str(root / SCRIPT.name)], env=env,
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 42, result.stderr)
            return result.stdout.strip()

    def test_default_download_uses_official_https(self):
        self.assertEqual(self.first_repository(),
                         'https://download.virtualbox.org/virtualbox/rpm/el/@{os_ver}/@{arch}')

    def test_configured_upstream_reaches_downloader(self):
        self.assertEqual(self.first_repository('https://mirror.example/virtualbox'),
                         'https://mirror.example/virtualbox/rpm/el/@{os_ver}/@{arch}')
