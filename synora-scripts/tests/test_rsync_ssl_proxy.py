import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('rsync_ssl_proxy', Path(__file__).resolve().parents[1] / 'helpers/rsync_ssl_proxy.py')
proxy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proxy)


class ProxyTests(unittest.TestCase):
    def test_tls_connect_keeps_verification_and_hides_password(self):
        args, env = proxy.command(['s_client', '-verify_return_error', '-verify_hostname', 'repos.fyralabs.com'],
                                  {'HTTPS_PROXY': 'http://user:p%40ss@proxy.test:14000'})
        self.assertIn('proxy.test:14000', args)
        self.assertIn('-verify_hostname', args)
        self.assertNotIn('p@ss', args)
        self.assertEqual(env['SYNORA_RSYNC_PROXY_PASSWORD'], 'p@ss')

    def test_missing_or_unsupported_proxy_fails_closed(self):
        for env in ({}, {'HTTPS_PROXY': 'https://proxy.test'}, {'ALL_PROXY': 'socks5://proxy.test'}):
            with self.assertRaises(ValueError):
                proxy.command(['s_client'], env)

    def test_ipv6_proxy(self):
        args, _ = proxy.command(['s_client'], {'HTTP_PROXY': 'http://[::1]:14000'})
        self.assertIn('[::1]:14000', args)
