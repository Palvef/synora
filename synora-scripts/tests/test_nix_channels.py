import importlib.util
import lzma
import os
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import patch

SCRIPT=Path(__file__).resolve().parents[1]/'nix-channels.py'

def load(root):
    spec=importlib.util.spec_from_file_location('nix_channels_test',SCRIPT)
    mod=importlib.util.module_from_spec(spec)
    with patch.dict(os.environ,{'SYNORA_STORAGE':str(root),'NIX_MIRROR_RETAIN_DAYS':'14'}):
        spec.loader.exec_module(mod)
    return mod

class NixRetentionTests(unittest.TestCase):
    def test_first_sync_gc_is_empty_and_safe(self):
        with tempfile.TemporaryDirectory() as d:
            mod=load(Path(d))
            mod.garbage_collect()

    def test_failed_closure_never_deletes_existing_cache(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);release=root/'releases'/'nixos-test@current';release.mkdir(parents=True)
            (release/'binary-cache-url').write_text('https://example.test/store')
            (release/'.sync-complete').touch()
            (release/'.released-time').write_text(datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M:%S'))
            (release/'store-paths.xz').write_bytes(lzma.compress(b'/nix/store/abc-package\n'))
            (root/'nixos-test').symlink_to('releases/nixos-test@current')
            store=root/'store';(store/'nar').mkdir(parents=True)
            (store/'abc.narinfo').write_text('URL: nar/abc.nar.xz\n')
            payload=store/'nar'/'abc.nar.xz';payload.write_bytes(b'existing')
            mod=load(root)
            with patch.object(mod.subprocess,'run',return_value=subprocess.CompletedProcess([],1,b'')),self.assertRaises(RuntimeError):
                mod.garbage_collect()
            self.assertTrue(payload.exists())
            self.assertTrue((store/'abc.narinfo').exists())

    def test_failed_channel_update_is_not_reported_as_success(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            release = root/'releases'/'nixos-test@new'
            release.mkdir(parents=True)
            (release/'.original-binary-cache-url').write_text('https://cache.example.test')
            (release/'store-paths.xz').write_bytes(lzma.compress(b'/nix/store/abc-pkg\n'))
            (root/'.nixos-test.update').symlink_to('releases/nixos-test@new')
            mod = load(root)
            with patch.object(mod, 'download'), patch.object(mod.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1, b'')):
                mod.update_channels(['nixos-test'])
            self.assertTrue(mod.failure)
            self.assertFalse((root/'nixos-test').exists())
            self.assertFalse((release/'.sync-complete').exists())

    def test_retention_keeps_published_and_recent_releases(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            store = root / 'store'
            (store / 'nar').mkdir(parents=True)
            now = datetime.now(timezone.utc)
            releases = {}
            for name, age in [('current', 30), ('recent', 10), ('expired', 20)]:
                release = root / 'releases' / ('nixos-test@' + name)
                release.mkdir(parents=True)
                (release / '.sync-complete').touch()
                (release / 'binary-cache-url').write_text('https://example.test/store')
                (release / '.released-time').write_text((now-timedelta(days=age)).strftime('%Y-%m-%d %H:%M:%S'))
                (release / 'store-paths.xz').write_bytes(lzma.compress(('/nix/store/'+name+'-pkg\n').encode()))
                (store / (name+'.narinfo')).write_text('URL: nar/'+name+'.nar.xz\n')
                (store / 'nar' / (name+'.nar.xz')).write_bytes(b'cache')
                releases[name] = release
            # Unreferenced narinfo can point to a NAR still used by the current channel.
            (store / 'alias.narinfo').write_text('URL: nar/current.nar.xz\n')
            (root/'nixos-test').symlink_to('releases/nixos-test@current')
            mod = load(root)
            def closure(args, **kwargs):
                return subprocess.CompletedProcess(args, 0, args[-1]+b'\n')
            with patch.object(mod.subprocess, 'run', side_effect=closure):
                mod.garbage_collect()
            for name in ['current', 'recent']:
                self.assertTrue(releases[name].exists())
                self.assertTrue((store/'nar'/(name+'.nar.xz')).exists())
            self.assertFalse(releases['expired'].exists())
            self.assertFalse((store/'nar'/'expired.nar.xz').exists())
            self.assertFalse((store/'alias.narinfo').exists())

    def test_interrupted_downloads_are_not_collected(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root/'.nixos-test.update').symlink_to('releases/pending')
            (root/'store'/'nar').mkdir(parents=True)
            cached = root/'store'/'nar'/'partial.nar.xz'
            cached.write_bytes(b'cached')
            load(root).garbage_collect()
            self.assertTrue(cached.exists())

if __name__=='__main__': unittest.main()
