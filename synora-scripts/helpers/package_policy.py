"""Shared default exclusions for debug symbols and test packages."""
import fnmatch
import os
from pathlib import Path

TOKENS = ('dbg', 'debug', 'dbgsym', 'debuginfo', 'debugsource', 'test', 'tests', 'testing', 'testsuite')
NAME_PATTERNS = tuple(p for token in TOKENS for p in (token, token+'-*', '*-'+token, '*-'+token+'-*'))

def excluded(name, section=''):
    patterns = NAME_PATTERNS + tuple(p.strip() for p in os.getenv('SYNC_EXCLUDE_PACKAGES', '').split(',') if p.strip())
    return any(fnmatch.fnmatchcase(name.lower(), p) for p in patterns) or section.lower().split('/')[-1] in TOKENS

def package_name(path):
    name = Path(path).name
    if name.endswith(('.deb', '.udeb', '.ddeb')):
        return name.split('_', 1)[0]
    if name.endswith('.rpm'):
        # RPM name-version-release.arch; versions/releases cannot contain '-'.
        return name.rsplit('-', 2)[0]
    return None

def excluded_file(path):
    name = package_name(path)
    return str(path).endswith('.ddeb') or (name is not None and excluded(name))

def prune_excluded(root):
    count = 0
    for path in root.rglob('*'):
        if path.is_file() and excluded_file(path) and not path.is_symlink():
            if not path.resolve().is_relative_to(root.resolve()):
                raise RuntimeError('Package cleanup path escapes repository')
            path.unlink()
            count += 1
    return count
