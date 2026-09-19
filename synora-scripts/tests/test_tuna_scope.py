"""Verify wrapper scopes without downloading packages or contacting upstreams."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class TunaScopeTests(unittest.TestCase):
    def calls(self, script, extra_env=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copy(ROOT / script, root / script)
            (root / 'helpers').mkdir()
            (root / 'helpers/size-sum.sh').write_text('#!/bin/sh\nexit 0\n')
            (root / 'helpers/size-sum.sh').chmod(0o755)
            recorder = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
with open(os.environ['CALLS'], 'a') as f:
    f.write(json.dumps([Path(sys.argv[0]).name, *sys.argv[1:-1]]) + '\\n')
Path(sys.argv[-1]).mkdir(parents=True, exist_ok=True)
'''
            for helper in ('apt-sync.py', 'yum-sync.py'):
                (root / helper).write_text(recorder)
                (root / helper).chmod(0o755)
            (root / 'curl').write_text('#!/bin/sh\nprintf "Codename: llvm-toolchain-future\\n"\n')
            (root / 'curl').chmod(0o755)
            env = {'PATH': str(root) + ':' + os.defpath, 'CALLS': str(root / 'calls'),
                   'SYNORA_STORAGE': str(root / 'data'), 'SYNORA_UPSTREAM': 'https://repo.test'}
            env.update(extra_env or {})
            subprocess.run(['bash', str(root / script)], env=env, check=True, capture_output=True)
            return [json.loads(line) for line in (root / 'calls').read_text().splitlines()]

    def test_mongodb_scopes_are_per_distribution(self):
        calls = self.calls('mongodb.sh')
        self.assertEqual(calls[0][3:6], ['@rhel-current', '8.0,7.0,6.0,5.0,4.4,4.2', 'x86_64'])
        for call, distro, components, arches in [
            (calls[1], 'ubuntu-lts', 'multiverse', 'amd64,i386,arm64'),
            (calls[2], 'debian-current', 'main', 'amd64,i386'),
        ]:
            self.assertEqual(call[3].split(','), [f'@{{{distro}}}/mongodb-org/{v}' for v in ['8.0','7.0','6.0','5.0','4.4','4.2']])
            self.assertEqual(call[4:6], [components, arches])

    def test_mysql_matches_components_and_arches(self):
        calls = self.calls('mysql.sh')
        for call, suite in zip(calls, ['@ubuntu-lts', '@debian-current']):
            self.assertEqual(call[3:6], [suite, 'mysql-tools,mysql-8.0,mysql-8.4-lts', 'amd64,i386'])
        self.assertEqual(calls[2][2:5], ['@rhel-current', 'mysql-connectors-community,mysql-tools-community,mysql-8.0-community,mysql-8.4-community', 'x86_64,aarch64'])

    def test_influxdata_keeps_repaired_rpm_endpoint(self):
        calls = self.calls('influxdata.sh')
        self.assertEqual(calls[0][3:6], ['stable', 'main', 'amd64,i386,armhf,arm64'])
        self.assertEqual(calls[1][2:5], ['@debian-current,@ubuntu-lts', 'stable', 'amd64,i386,armhf,arm64'])
        self.assertEqual(calls[2][1:5], ['https://repo.test/stable/@{arch}/main/', 'stable', 'influxdata', 'x86_64'])

    def test_xanmod_scope(self):
        self.assertEqual(self.calls('xanmod.sh')[0][3:6], ['@ubuntu-lts,@debian-current', 'main,non-free', 'amd64,i386'])

    def test_llvm_distros_and_dynamic_toolchain_versions(self):
        calls = self.calls('llvm-apt.sh')
        self.assertEqual([call[2].split('/')[-1] for call in calls], ['jammy','noble','resolute','bullseye','bookworm','trixie'])
        self.assertTrue(all(call[3:6] == ['llvm-toolchain-future', 'main', 'amd64,arm64'] for call in calls))

    def test_elastic_major_scope_and_override(self):
        calls = self.calls('elastic.sh')
        self.assertEqual([call[2].split('/')[-2] for call in calls[:-1]], ['6.x','7.x','8.x','9.x'])
        self.assertEqual(calls[-1][3:5], ['6.x,7.x,8.x,9.x', 'x86_64'])
        calls = self.calls('elastic.sh', {'ELASTIC_MAJORS': '10.x'})
        self.assertEqual(calls[-1][3:5], ['10.x', 'x86_64'])


if __name__ == '__main__':
    unittest.main()
