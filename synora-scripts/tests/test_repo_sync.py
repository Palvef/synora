import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch, Mock

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'helpers'))
import repo_discovery as discovery

def module(name):
    spec=importlib.util.spec_from_file_location(name,ROOT/(name+'.py'))
    result=importlib.util.module_from_spec(spec);spec.loader.exec_module(result);return result

class DiscoveryTests(unittest.TestCase):
    def tearDown(self): discovery.directories.cache_clear();discovery.release_fields.cache_clear()
    def test_explicit_release_aliases_do_not_fetch(self):
        discovery.distro_versions.cache_clear(); discovery.rpm_versions.cache_clear()
        try:
            with patch.dict('os.environ', {'SYNC_RELEASES_UBUNTU_LTS': 'jammy,noble,resolute', 'SYNC_RELEASES_RHEL_CURRENT': '9,10', 'SYNC_RELEASES_FEDORA_CURRENT': '41,42'}, clear=True), patch.object(discovery.requests, 'get') as fetch:
                self.assertEqual(discovery.distro_versions('ubuntu-lts'), ['jammy','noble','resolute'])
                self.assertEqual(discovery.rpm_versions('@rhel-current'), ['9','10'])
                self.assertEqual(discovery.rpm_versions('@fedora-current'), ['41','42'])
                fetch.assert_not_called()
        finally:
            discovery.distro_versions.cache_clear(); discovery.rpm_versions.cache_clear()

    def test_unconfigured_release_aliases_keep_discovery(self):
        discovery.rpm_versions.cache_clear()
        try:
            with patch.dict('os.environ', {}, clear=True), patch.object(discovery, 'directories', return_value=['8','9','10','11']):
                self.assertEqual(discovery.rpm_versions('@rhel-current'), ['8','9','10','11'])
        finally:
            discovery.rpm_versions.cache_clear()

    def test_release_override_rejects_empty_or_unsafe_scope(self):
        for raw in ('', '9,,10', '../9', '9/*'):
            with patch.dict('os.environ', {'SYNC_RELEASES_RHEL_CURRENT': raw}, clear=True), self.assertRaises(ValueError):
                discovery.configured_releases('@rhel-current')

    def test_selection_defaults_and_exclusion_precedence(self):
        from repo_selection import Selection
        with patch.dict('os.environ', {}, clear=True):
            self.assertTrue(Selection().allows('bookworm', 'arm64', 'main'))
        with patch.dict('os.environ', {'SYNC_VERSIONS':'book*,trixie', 'SYNC_EXCLUDE_ARCHES':'arm*', 'SYNC_EXCLUDE_COMPONENTS':'testing'}, clear=True):
            selector=Selection()
            self.assertTrue(selector.allows('bookworm','amd64','main'))
            self.assertFalse(selector.allows('bullseye','amd64','main'))
            self.assertFalse(selector.allows('bookworm','arm64','main'))
            self.assertFalse(selector.allows('trixie','amd64','testing'))

    def test_apt_selection_does_not_delete_excluded_architecture(self):
        apt=module('apt-sync')
        with tempfile.TemporaryDirectory() as dest, patch.dict('os.environ', {'SYNC_EXCLUDE_ARCHES':'arm64'}, clear=True), patch.object(sys,'argv',['apt-sync','--delete','https://repo.test','future','main','amd64,arm64',dest]), patch.object(apt,'apt_mirror',return_value=0) as mirror, patch.object(apt,'apt_delete_old_debs') as delete:
            apt.main()
            self.assertEqual(mirror.call_count,1)
            self.assertEqual(mirror.call_args.args[3],'amd64')
            delete.assert_not_called()

    def test_apt_explicitly_excluded_suite_needs_no_metadata(self):
        apt=module('apt-sync')
        with tempfile.TemporaryDirectory() as dest, patch.dict('os.environ', {'SYNC_EXCLUDE_VERSIONS':'*'}, clear=True), patch.object(sys,'argv',['apt-sync','https://repo.test','future','@auto','@auto',dest]), patch.object(apt,'release_fields') as fields, patch.object(apt,'apt_mirror') as mirror:
            apt.main()
            fields.assert_not_called()
            mirror.assert_not_called()

    def test_directory_scope_and_empty_fail_closed(self):
        response=Mock(text='<a href="../">parent</a><a href="new-release/">new</a><a href="https://evil.test/x/">evil</a><a href="../../bad/">escape</a>')
        with patch.object(discovery.requests,'get',return_value=response):
            self.assertEqual(discovery.directories('https://repo.test/dists'),['new-release'])
        discovery.directories.cache_clear();response.text='<html>Login</html>'
        with patch.object(discovery.requests,'get',return_value=response),self.assertRaises(RuntimeError): discovery.directories('https://repo.test/dists')
    def test_directory_label_without_slash_in_href(self):
        response=Mock(text='<a href="/stable">..</a><a href="/stable/futurearch">futurearch/</a><a href="/stable/readme">readme</a><a href="https://evil.test/x">x/</a>')
        with patch.object(discovery.requests,'get',return_value=response):
            self.assertEqual(discovery.directories('https://repos.influxdata.com/stable/'), ['futurearch'])

    def test_xanmod_official_codenames_are_discovered(self):
        response=Mock(text='<p>Supported distribution codenames: <b>future*</b>, next and rolling.</p>')
        with patch.object(discovery.requests,'get',return_value=response):
            self.assertEqual(discovery.apt_suites('https://deb.xanmod.org','@xanmod'), ['future','next','rolling'])

    def test_yum_failure_does_not_block_next_repository(self):
        import subprocess
        import mongodb_rpm
        yum=module('yum-sync')
        matrix=[({'os_ver':'1','comp':'broken','arch':'x86_64'},'https://repo.test/broken'),({'os_ver':'1','comp':'healthy','arch':'x86_64'},'https://repo.test/healthy')]
        response=Mock(status_code=200,content=b'<repomd/>')
        calls=[]
        def execute(args, **kwargs):
            calls.append(args[0])
            if len(calls)==1: raise subprocess.CalledProcessError(1,args)
            return subprocess.CompletedProcess(args,0)
        with tempfile.TemporaryDirectory() as d, patch.object(sys,'argv',['yum-sync','https://repo.test/@{comp}','1','broken,healthy','x86_64','@{comp}',d]), patch.object(yum,'repository_matrix',return_value=matrix), patch.object(yum.requests,'get',return_value=response), patch.object(mongodb_rpm,'needs_repair',return_value=False), patch.object(yum.sp,'run',side_effect=execute), patch.object(yum,'calc_repo_size') as calc:
            with self.assertRaises(SystemExit): yum.main()
            self.assertEqual(calls,['dnf','dnf','createrepo_c'])
            self.assertEqual(calc.call_count,1)

    def test_future_nested_versions(self):
        def listing(url):return ['future-os'] if url.rstrip('/').endswith('/dists') else ['12.0','13.0']
        with patch.object(discovery,'directories',side_effect=listing):
            self.assertEqual(discovery.apt_suites('https://repo.test','@auto/mongodb-org/@auto'),['future-os/mongodb-org/12.0','future-os/mongodb-org/13.0'])
    def test_release_metadata_validation(self):
        r=Mock(status_code=200,text='Components: main next\nArchitectures: amd64 arm64\n')
        with patch.object(discovery.requests,'get',return_value=r):self.assertEqual(discovery.release_fields('https://repo.test','future')['Components'],'main next')
    def test_distro_inventory_handles_unknown_eol_and_future_releases(self):
        response=Mock(text='version,codename,series,created,release,eol,eol-lts\n1,Old,old,2000-01-01,2000-01-01,2001-01-01,2002-01-01\n2,Current,current,2020-01-01,2020-01-01,,\n3,Future,future,2099-01-01,2099-01-01,,\n')
        discovery.distro_versions.cache_clear()
        with patch.object(discovery.requests,'get',return_value=response):
            self.assertEqual(discovery.distro_versions('debian-latest'),['current'])
        discovery.distro_versions.cache_clear()
    def test_rpm_inventory_does_not_mix_epel_into_fedora(self):
        response=Mock();response.json.return_value={'releases':[{'name':'F99','version':'99'},{'name':'EPEL12','version':'12'}]}
        discovery.rpm_versions.cache_clear()
        with patch.object(discovery.requests,'get',return_value=response):
            self.assertEqual(discovery.rpm_versions('@fedora-current'),['99'])
        discovery.rpm_versions.cache_clear()
    def test_discovery_server_error_is_not_an_empty_success(self):
        response=Mock();response.raise_for_status.side_effect=RuntimeError('503 upstream unavailable')
        with patch.object(discovery.requests,'get',return_value=response),self.assertRaises(RuntimeError):
            discovery.directories('https://repo.test/dists')
    def test_release_rejects_path_escape(self):
        response=Mock(status_code=200,text='Components: ../../escape\nArchitectures: amd64\n')
        with patch.object(discovery.requests,'get',return_value=response),self.assertRaises(RuntimeError):
            discovery.release_fields('https://repo.test','future')
    def test_yum_future_version_matrix(self):
        yum=module('yum-sync')
        with patch.object(yum,'directories',return_value=['12','13']):
            result=list(yum.repository_matrix('https://repo.test/el/@{os_ver}/@{arch}', ['@auto'],['unused'],['x86_64']))
        self.assertEqual([x[0]['os_ver'] for x in result],['12','13'])
    def test_primary_selected_by_repomd_not_glob(self):
        import gzip
        yum=module('yum-sync')
        with tempfile.TemporaryDirectory() as d:
            p=Path(d);(p/'repodata').mkdir()
            (p/'repodata/old-primary.xml.gz').write_bytes(b'invalid')
            (p/'repodata/current.xml.gz').write_bytes(gzip.compress(b'<metadata xmlns="http://linux.duke.edu/metadata/common"><package><size package="123"/></package></metadata>'))
            (p/'repodata/repomd.xml').write_text('<repomd xmlns="http://linux.duke.edu/metadata/repo"><data type="primary"><location href="repodata/current.xml.gz"/></data></repomd>')
            yum.calc_repo_size(p);self.assertEqual(yum.REPO_STAT[str(p)],(123,1))
    def test_invalid_primary_fails(self):
        yum=module('yum-sync')
        with tempfile.TemporaryDirectory() as d:
            p=Path(d);(p/'repodata').mkdir();(p/'repodata/repomd.xml').write_text('<repomd/>')
            with self.assertRaises(RuntimeError):yum.calc_repo_size(p)

if __name__=='__main__':unittest.main()

class AptCleanupTests(unittest.TestCase):
    def test_component_release_does_not_disable_package_cleanup(self):
        apt = module('apt-sync')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            component = root/'dists/stable/main/binary-amd64/Release'
            component.parent.mkdir(parents=True)
            component.write_text('Archive: stable\nComponent: main\nArchitecture: amd64\n')
            self.assertEqual(apt.retained_apt_suites(root, {'stable'}, 'https://repo.test'), set())
    def test_only_confirmed_missing_suites_are_removed(self):
        apt = module('apt-sync')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = root/'dists/obsolete/Release'
            release.parent.mkdir(parents=True)
            release.write_text('Components: main\nArchitectures: amd64\n')
            response = Mock(status_code=404)
            with patch.object(apt.requests, 'get', return_value=response):
                self.assertEqual(apt.retained_apt_suites(root, {'stable'}, 'https://repo.test', True), set())
            self.assertFalse(release.parent.exists())
    def test_network_failure_preserves_old_suite(self):
        apt = module('apt-sync')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = root/'dists/obsolete/Release'
            release.parent.mkdir(parents=True)
            release.write_text('Components: main\nArchitectures: amd64\n')
            response = Mock(status_code=503)
            response.raise_for_status.side_effect = RuntimeError('upstream unavailable')
            with patch.object(apt.requests, 'get', return_value=response), self.assertRaises(RuntimeError):
                apt.retained_apt_suites(root, {'stable'}, 'https://repo.test', True)
            self.assertTrue(release.exists())
