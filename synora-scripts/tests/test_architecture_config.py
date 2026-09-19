import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

from test_repo_sync import module
from repo_selection import Selection


class ArchitectureConfigurationTests(unittest.TestCase):
    def test_protocol_overrides_are_independent(self):
        with patch.dict(os.environ, {'SYNC_APT_ARCHES': 'amd64,arm64', 'SYNC_YUM_ARCHES': 'x86_64'}, clear=True):
            self.assertEqual(Selection('APT').architectures, 'amd64,arm64')
            self.assertEqual(Selection('YUM').architectures, 'x86_64')
        with patch.dict(os.environ, {}, clear=True):
            self.assertIsNone(Selection('APT').architectures)
            self.assertFalse(Selection('APT').active)

    def test_mongodb_ubuntu_and_debian_have_separate_scopes(self):
        env = {'SYNC_APT_ARCHES': 'all', 'SYNC_APT_ARCHES_BY_PATH': json.dumps({
            '/apt/ubuntu': 'amd64,i386,arm64', '/apt/debian': 'amd64,i386'})}
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(Selection('APT', 'https://repo.mongodb.org/apt/ubuntu/').architectures, 'amd64,i386,arm64')
            self.assertEqual(Selection('APT', 'https://repo.mongodb.org/apt/debian').architectures, 'amd64,i386')
            self.assertEqual(Selection('APT', 'https://repo.mongodb.org/other').architectures, 'all')

    def test_invalid_overrides_fail_before_sync(self):
        for value in ('', 'amd64,,arm64', '../amd64', '*'):
            with patch.dict(os.environ, {'SYNC_APT_ARCHES': value}, clear=True), self.assertRaises(ValueError):
                Selection('APT')
        for value in ('[]', '{"apt": "amd64"}', '{"/apt": []}', '{"/apt":"amd64","/apt/":"arm64"}'):
            with patch.dict(os.environ, {'SYNC_APT_ARCHES_BY_PATH': value}, clear=True), self.assertRaises(ValueError):
                Selection('APT')

    def test_apt_override_replaces_wrapper_arches_and_keeps_cleanup_safe(self):
        apt = module('apt-sync')
        with tempfile.TemporaryDirectory() as dest, patch.dict(os.environ, {'SYNC_APT_ARCHES': 'amd64,arm64', 'SYNC_EXCLUDE_ARCHES': 'amd64'}, clear=True), patch.object(sys, 'argv', ['apt-sync', '--delete', 'https://repo.test', 'next', 'main', 'i386', dest]), patch.object(apt, 'apt_mirror', return_value=0) as mirror, patch.object(apt, 'apt_delete_old_debs') as delete:
            apt.main()
            self.assertEqual(mirror.call_count, 1)
            self.assertEqual(mirror.call_args.args[3], 'arm64')
            delete.assert_not_called()

    def test_unconfigured_apt_discovers_new_arches_and_suites(self):
        apt = module('apt-sync')
        with tempfile.TemporaryDirectory() as dest, patch.dict(os.environ, {}, clear=True), patch.object(sys, 'argv', ['apt-sync', 'https://repo.test', '@auto', '@auto', '@auto', dest]), patch.object(apt, 'apt_suites', return_value=['next']) as suites, patch.object(apt, 'release_fields', return_value={'Components': 'main', 'Architectures': 'amd64 loong64'}), patch.object(apt, 'apt_mirror', return_value=0) as mirror:
            apt.main()
            suites.assert_called_once_with('https://repo.test', '@auto')
            self.assertEqual([call.args[3] for call in mirror.call_args_list], ['amd64', 'loong64'])

    def test_yum_override_changes_requested_matrix_before_fetch(self):
        yum = module('yum-sync')
        response = Mock(status_code=200, content=b'<repomd/>')
        def matrix(base, versions, components, arches):
            self.assertEqual(versions, ['@auto'])
            self.assertEqual(arches, ['aarch64'])
            return [({'os_ver': 'future', 'comp': 'main', 'arch': arches[0]}, 'https://repo.test/future/aarch64')]
        with tempfile.TemporaryDirectory() as dest, patch.dict(os.environ, {'SYNC_YUM_ARCHES': 'aarch64'}, clear=True), patch.object(sys, 'argv', ['yum-sync', '--dry-run', 'https://repo.test/@{os_ver}/@{arch}', '@auto', 'main', 'x86_64', '@{os_ver}-@{arch}', dest]), patch.object(yum, 'repository_matrix', side_effect=matrix) as discover, patch.object(yum.requests, 'get', return_value=response) as fetch:
            yum.main()
            self.assertEqual(discover.call_count, 1)
            self.assertEqual(fetch.call_args.args[0], 'https://repo.test/future/aarch64/repodata/repomd.xml')
            self.assertFalse((Path(dest) / '.yum-sync.lock').exists())


if __name__ == '__main__':
    unittest.main()
