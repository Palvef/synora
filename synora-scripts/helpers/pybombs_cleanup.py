"""Remove unused PyBOMBS assets only after a complete, successful recipe sync."""
from pathlib import Path
import posixpath
import shutil
import sys


def cleanup(root):
    failure = root/'failed.log'
    if failure.exists() and failure.stat().st_size:
        return 0
    inventory = root/'recipes-origin.urls'
    if not inventory.is_file():
        raise RuntimeError('Missing PyBOMBS source inventory; cleanup refused')
    expected = set()
    for line in inventory.read_text().splitlines():
        if not line.strip():
            continue
        protocol, separator, url = line.strip().partition('+')
        if not separator or protocol not in ('git', 'wget', 'svn'):
            raise RuntimeError('Invalid PyBOMBS source inventory; cleanup refused')
        name = posixpath.basename(posixpath.dirname(url))+'_'+posixpath.basename(url)
        expected.add((protocol, name))
    if not expected:
        raise RuntimeError('Empty PyBOMBS source inventory; cleanup refused')
    removed = 0
    for protocol in ('git', 'wget', 'svn'):
        directory = root/protocol
        if not directory.exists():
            continue
        if directory.is_symlink():
            raise RuntimeError('PyBOMBS asset root is a symlink')
        for path in directory.iterdir():
            if (protocol, path.name) in expected:
                continue
            if path.is_symlink() or path.is_file():
                path.unlink()
            elif path.is_dir():
                shutil.rmtree(path)
            removed += 1
    return removed

if __name__ == '__main__':
    print('Removed obsolete PyBOMBS assets:', cleanup(Path(sys.argv[1])))
