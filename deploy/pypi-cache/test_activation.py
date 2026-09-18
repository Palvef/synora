import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('activate', Path(__file__).with_name('activate.py'))
activate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(activate)


class ActivationTests(unittest.TestCase):
    def test_replace_only_old_pypi_locations(self):
        original = '''server {
 location /other/ { return 200; }
 location /pypi/ {
   if ($nyistnet != 1) { return 302 https://example.test$request_uri; }
   proxy_pass https://example.test;
 }
 location @pypi_404 { return 404; }
 include /etc/nginx/snippets/lua_common.conf;
}'''
        changed = activate.local_routes(original)
        self.assertIn('location /other/ { return 200; }', changed)
        self.assertIn('include /etc/nginx/snippets/synora-pypi-server.conf;', changed)
        self.assertIn('include /etc/nginx/snippets/lua_common.conf;', changed)
        self.assertNotIn('proxy_pass', changed)
        self.assertNotIn('@pypi_404', changed)
        self.assertEqual(activate.local_routes(changed), changed)

    def test_unexpected_config_rejected(self):
        with self.assertRaises(ValueError):
            activate.local_routes('server { location / { return 200; } }')


if __name__ == '__main__':
    unittest.main()
