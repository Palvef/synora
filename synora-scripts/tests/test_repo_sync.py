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
    def test_directory_scope_and_empty_fail_closed(self):
        response=Mock(text='<a href="../">parent</a><a href="new-release/">new</a><a href="https://evil.test/x/">evil</a><a href="../../bad/">escape</a>')
        with patch.object(discovery.requests,'get',return_value=response):
            self.assertEqual(discovery.directories('https://repo.test/dists'),['new-release'])
        discovery.directories.cache_clear();response.text='<html>Login</html>'
        with patch.object(discovery.requests,'get',return_value=response),self.assertRaises(RuntimeError): discovery.directories('https://repo.test/dists')
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
            self.assertEqual(discovery.distro_versions('debian-current'),['current'])
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
