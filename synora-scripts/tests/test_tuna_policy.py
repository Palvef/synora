import json
import os
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import Mock, patch

from test_repo_sync import module
import tuna_scope
from repo_selection import Selection

APT = 'OS_TEMPLATE = {"ubuntu-lts": ["jammy", "noble"], "debian-current": ["bookworm", "trixie"]}'
YUM = 'OS_TEMPLATE = {"rhel-current": ["9", "10"]}'
MONGO = '''MONGO_VERSIONS=("8.0" "7.0")
STABLE_VERSION="8.0"
"$yum_sync" "${BASE_URL}/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}/" @rhel-current "$components" x86_64 "el@{os_ver}-@{comp}" "$YUM_PATH"
"$apt_sync" --delete "$BASE_URL/apt/ubuntu" "${components:1}" multiverse amd64,i386,arm64 "$UBUNTU_PATH"
"$apt_sync" --delete "$BASE_URL/apt/debian" "${components:1}" main amd64,i386 "$DEBIAN_PATH"
'''
PROXMOX = '''"$apt_sync" --delete "${BASE_URL}/debian/pve" @debian-current pve-no-subscription,pvetest amd64 "$PVE_PATH"
"$apt_sync" --delete "${BASE_URL}/debian/pbs" @debian-current pbs-no-subscription amd64 "$PBS_PATH"
"$apt_sync" --delete "${BASE_URL}/debian/pbs-client" @debian-current main amd64 "$PBS_CLIENT_PATH"
"$apt_sync" --delete "${BASE_URL}/debian/pmg" @debian-current pmg-no-subscription amd64 "$PMG_PATH"
'''


def scope(repo='mongodb', source=MONGO):
    return tuna_scope.parse_scope(repo, source, APT, YUM, 'a' * 40)


class TunaPolicyTests(unittest.TestCase):
    def selection(self, root, protocol, path, data=None):
        p = Path(root)/'policy.json'
        p.write_text(json.dumps(data or scope()))
        with patch.dict(os.environ, {'SYNC_SCOPE_POLICY': 'tuna', 'SYNORA_TUNA_SCOPE_FILE': str(p)}, clear=True):
            return Selection(protocol, 'https://repo.mongodb.org'+path)

    def test_mongodb_rejects_historical_test_and_duplicate_branches(self):
        with tempfile.TemporaryDirectory() as root:
            rpm = self.selection(root, 'YUM', '/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}')
            self.assertTrue(rpm.allows('9','x86_64','8.0'))
            for version, arch, component in [('9Server','x86_64','8.0'),('5','x86_64','7.0'),('9','aarch64','8.0'),('9','x86_64','development'),('9','x86_64','testing'),('9','x86_64','9.0')]:
                self.assertFalse(rpm.allows(version,arch,component))
            ubuntu = self.selection(root, 'APT', '/apt/ubuntu')
            debian = self.selection(root, 'APT', '/apt/debian')
            self.assertTrue(ubuntu.allows('noble/mongodb-org/8.0','arm64','multiverse'))
            self.assertFalse(ubuntu.allows('bionic/mongodb-org/8.0','amd64','multiverse'))
            self.assertFalse(debian.allows('trixie/mongodb-org/8.0','arm64','main'))

    def test_tuna_new_version_is_followed_without_local_version_edit(self):
        data = scope(source=MONGO.replace('"8.0" "7.0"', '"11.0" "8.0" "7.0"'))
        with tempfile.TemporaryDirectory() as root:
            rpm = self.selection(root, 'YUM', '/yum/redhat', data)
            self.assertTrue(rpm.allows('10','x86_64','11.0'))

    def test_proxmox_policy_and_operator_test_exclusion_intersect(self):
        with tempfile.TemporaryDirectory() as root:
            p=Path(root)/'policy.json';p.write_text(json.dumps(scope('proxmox',PROXMOX)))
            env={'SYNC_SCOPE_POLICY':'tuna','SYNORA_TUNA_SCOPE_FILE':str(p),'SYNC_EXCLUDE_COMPONENTS':'pvetest'}
            with patch.dict(os.environ,env,clear=True):
                selection=Selection('APT','http://download.proxmox.com/debian/pve')
            self.assertTrue(selection.allows('trixie','amd64','pve-no-subscription'))
            self.assertFalse(selection.allows('buster','amd64','pve-no-subscription'))
            self.assertFalse(selection.allows('trixie','amd64','pvetest'))

    def test_missing_or_unrecognized_policy_fails_closed(self):
        for env in ({'SYNC_SCOPE_POLICY':'tuna'},{'SYNC_SCOPE_POLICY':'typo'}):
            with patch.dict(os.environ,env,clear=True), self.assertRaises(ValueError):Selection('APT','https://repo.test')
        with tempfile.TemporaryDirectory() as root, self.assertRaises(ValueError):
            self.selection(root,'APT','/unexpected')
        with self.assertRaises(ValueError):scope(source=MONGO.replace('("8.0" "7.0")','($(touch /tmp/unsafe))'))

    def test_yum_does_not_traverse_excluded_branches(self):
        yum=module('yum-sync')
        def listing(url):
            if url.endswith('/redhat'):return ['5','9','9Server']
            if url.endswith('/9/mongodb-org'):return ['8.0','testing','development']
            if url.endswith('/9/mongodb-org/8.0'):return ['x86_64','aarch64']
            self.fail('Unselected branch traversed: '+url)
        with tempfile.TemporaryDirectory() as root:
            selection=self.selection(root,'YUM','/yum/redhat')
            with patch.object(yum,'directories',side_effect=listing):
                rows=list(yum.repository_matrix('https://repo.mongodb.org/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}', ['@auto'],['@auto'],['@auto'],selection))
            self.assertEqual(len(rows),1)
            self.assertEqual(rows[0][0],{'os_ver':'9','comp':'8.0','arch':'x86_64'})

    def test_source_python_is_parsed_not_executed(self):
        data=tuna_scope.parse_scope('mongodb',MONGO,APT+'\nraise RuntimeError("must not execute")',YUM,'b'*40)
        self.assertEqual(data['source_commit'],'b'*40)

    def test_empty_or_invalid_tuna_templates_fail_closed(self):
        for apt in ('OS_TEMPLATE = {}', 'OS_TEMPLATE = load_remote()', 'OS_TEMPLATE = {"ubuntu-lts":["../escape"]}'):
            with self.assertRaises(ValueError):tuna_scope.parse_scope('mongodb',MONGO,apt,YUM,'c'*40)

    def test_fetch_pins_all_sources_to_one_commit(self):
        sha = 'd' * 40
        responses = [Mock(json=lambda: {'sha': sha}), Mock(text=MONGO), Mock(text=APT), Mock(text=YUM)]
        with patch.object(tuna_scope.requests, 'get', side_effect=responses) as get:
            self.assertEqual(tuna_scope.fetch_scope('mongodb')['source_commit'], sha)
        self.assertEqual(len(get.call_args_list), 4)
        for call in get.call_args_list[1:]:
            self.assertIn('/' + sha + '/', call.args[0])
        broken = Mock()
        broken.raise_for_status.side_effect = RuntimeError('offline')
        with patch.object(tuna_scope.requests, 'get', return_value=broken), self.assertRaises(RuntimeError):
            tuna_scope.fetch_scope('mongodb')

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

    def test_aliases_do_not_point_to_out_of_scope_local_version(self):
        with tempfile.TemporaryDirectory() as root:
            base=Path(root)
            for version in ['8.0','9.0']:
                p=base/'apt/ubuntu/dists/noble/mongodb-org'/version
                p.mkdir(parents=True);(p/'Release').write_text('valid')
                p=base/'yum'/('el9-'+version)/'repodata'
                p.mkdir(parents=True);(p/'repomd.xml').write_text('valid')
            tuna_scope.mongodb_aliases(base,scope())
            self.assertEqual(os.readlink(base/'apt/ubuntu/dists/noble/mongodb-org/stable'),'8.0')
            self.assertEqual(os.readlink(base/'yum/el9'),'el9-8.0')


if __name__=='__main__':unittest.main()
