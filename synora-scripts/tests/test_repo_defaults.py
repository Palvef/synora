import os
import tempfile
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

from test_repo_sync import module
import repo_defaults
import repo_discovery
from repo_selection import Selection


class RepositoryDefaultsTests(unittest.TestCase):
    def setUp(self):
        self.env = patch.dict(os.environ, {}, clear=True)
        self.env.start()

    def tearDown(self):
        self.env.stop()
        repo_discovery.rpm_versions.cache_clear()
        repo_discovery.distro_versions.cache_clear()

    def select(self, repo, protocol='APT', path='/'):
        with patch.dict(os.environ, {'SYNORA_REPOSITORY': repo}):
            return Selection(protocol, 'https://repo.test' + path)

    def test_architecture_defaults_inherit_and_override(self):
        self.assertEqual(self.select('elastic').architectures, 'amd64,i386')
        self.assertEqual(self.select('mysql', 'YUM').architectures, 'x86_64,aarch64')
        self.assertEqual(self.select('termux').architectures, 'aarch64,arm,i686,x86_64')
        self.assertEqual(self.select('mongodb', path='/apt/ubuntu').architectures, 'amd64,i386,arm64')
        self.assertEqual(self.select('mongodb', path='/apt/debian').architectures, 'amd64,i386')
        with patch.dict(os.environ, {'SYNC_APT_ARCHES': 'loong64'}):
            self.assertEqual(self.select('elastic').architectures, 'loong64')

    def test_mongodb_shared_groups_do_not_expand_scope(self):
        with patch.object(repo_discovery, 'rpm_versions', return_value=['9','10']), patch.object(repo_discovery, 'distro_versions', side_effect=lambda group: ['noble'] if group == 'ubuntu-lts' else ['trixie']):
            rpm = self.select('mongodb', 'YUM', '/yum/redhat/@{os_ver}')
            ubuntu = self.select('mongodb', path='/apt/ubuntu')
            debian = self.select('mongodb', path='/apt/debian')
        self.assertTrue(rpm.allows('9','x86_64','8.0'))
        for v,c in [('9Server','8.0'),('8','8.0'),('9','testing'),('9','development'),('9','8.3')]:
            self.assertFalse(rpm.allows(v,'x86_64',c))
        self.assertTrue(ubuntu.allows('noble/mongodb-org/8.0','arm64','multiverse'))
        self.assertFalse(ubuntu.allows('focal/mongodb-org/8.0','arm64','multiverse'))
        self.assertFalse(debian.allows('trixie/mongodb-org/8.0','arm64','main'))

    def test_job_exceptions_intersect_inherited_defaults(self):
        with patch.object(repo_discovery,'distro_versions',return_value=['trixie']), patch.dict(os.environ, {'SYNC_EXCLUDE_COMPONENTS':'pvetest'}):
            pve = self.select('proxmox', path='/debian/pve')
        self.assertTrue(pve.allows('trixie','amd64','pve-no-subscription'))
        self.assertFalse(pve.allows('trixie','amd64','pvetest'))
        self.assertFalse(pve.allows('buster','amd64','pve-no-subscription'))
        with patch.dict(os.environ, {'SYNC_EXCLUDE_VERSIONS':'stable'}):
            influx = self.select('influxdata','YUM','/stable')
        self.assertFalse(influx.matches('VERSIONS','stable'))

    def test_explicit_versions_replace_inherited_versions_but_exclusions_win(self):
        with patch.dict(os.environ, {'SYNC_VERSIONS':'next','SYNC_EXCLUDE_VERSIONS':'next'}), patch.object(repo_discovery,'distro_versions') as discover:
            selected=self.select('proxmox',path='/debian/pve')
        discover.assert_not_called()
        self.assertFalse(selected.matches('VERSIONS','next'))

    def test_central_file_can_override_release_groups_and_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            config=Path(tmp)/'defaults.toml'
            original=repo_defaults.DEFAULT_FILE.read_text()
            config.write_text(original.replace('\n[groups]\n', '\n[groups]\nrhel-current = ["11", "12"]\n'))
            with patch.dict(os.environ, {'SYNC_DEFAULTS_FILE':str(config)}):
                self.assertEqual(repo_defaults.group_values('rhel-current'),['11','12'])
            config.write_text('schema=1\n[groups]\nrhel-current=["../bad"]\n')
            with patch.dict(os.environ, {'SYNC_DEFAULTS_FILE':str(config)}), self.assertRaises(ValueError):
                repo_defaults.group_values('rhel-current')
        with patch.dict(os.environ, {'SYNORA_REPOSITORY':'typo'}), self.assertRaises(ValueError):
            Selection('APT','https://repo.test')

    def test_rhel_discovery_uses_shared_window_and_ignores_aliases(self):
        with patch.object(repo_discovery,'directories',return_value=['8','9','10','10.1','9Server','testing']):
            self.assertEqual(repo_discovery.rpm_versions('@rhel-current'),['9','10'])
        repo_discovery.rpm_versions.cache_clear()
        with patch.object(repo_discovery,'directories',return_value=['9','10','11']):
            self.assertEqual(repo_discovery.rpm_versions('@rhel-current'),['10','11'])

    def test_profile_filter_keeps_excluded_apt_content(self):
        apt=module('apt-sync')
        import sys
        with tempfile.TemporaryDirectory() as dest, patch.dict(os.environ,{'SYNORA_REPOSITORY':'elastic'}), patch.object(sys,'argv',['apt-sync','--delete','https://repo.test','stable','main','@auto',dest]), patch.object(apt,'apt_mirror',return_value=0) as mirror, patch.object(apt,'apt_delete_old_debs') as delete:
            apt.main()
            self.assertEqual([call.args[3] for call in mirror.call_args_list],['amd64','i386'])
            delete.assert_not_called()

    def test_proxmox_link_is_idempotent(self):
        wrapper = (Path(__file__).parents[1] / 'proxmox.sh').read_text()
        command = next(line for line in wrapper.splitlines() if line.startswith('ln '))
        with tempfile.TemporaryDirectory() as root:
            target = Path(root) / 'pve/dists'
            target.mkdir(parents=True)
            for _ in range(2):
                subprocess.run(['bash', '-ec', command], env={**os.environ, 'APT_PATH': root}, check=True)
            self.assertEqual(os.readlink(Path(root) / 'dists'), 'pve/dists')
            self.assertFalse((target / 'dists').is_symlink())


    def test_aliases_only_use_selected_products_with_metadata(self):
        from mongodb_aliases import mongodb_aliases
        with tempfile.TemporaryDirectory() as root:
            base=Path(root)
            for version in ['8.0','9.0']:
                p=base/'apt/ubuntu/dists/noble/mongodb-org'/version
                p.mkdir(parents=True);(p/'Release').write_text('valid')
                p=base/'yum'/('el9-'+version)/'repodata'
                p.mkdir(parents=True);(p/'repomd.xml').write_text('valid')
            with patch.dict(os.environ,{'SYNORA_REPOSITORY':'mongodb'}), patch.object(repo_discovery,'rpm_versions',return_value=['9','10']), patch.object(repo_discovery,'distro_versions',return_value=['noble']):
                mongodb_aliases(base)
            self.assertEqual(os.readlink(base/'apt/ubuntu/dists/noble/mongodb-org/stable'),'8.0')
            self.assertEqual(os.readlink(base/'yum/el9'),'el9-8.0')

    def test_every_wrapper_declares_a_known_profile(self):
        import re
        root=Path(__file__).parents[1]
        profiles=repo_defaults.configuration()['repositories']
        for p in root.glob('*.sh'):
            source=p.read_text()
            if 'apt-sync.py' in source or 'yum-sync.py' in source:
                match=re.search(r'^export SYNORA_REPOSITORY=([a-z0-9-]+)$',source,re.M)
                self.assertIsNotNone(match,p.name)
                self.assertIn(match[1],profiles)


if __name__ == '__main__': unittest.main()
