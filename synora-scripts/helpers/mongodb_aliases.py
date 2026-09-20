"""Publish stable aliases only to synchronized, selected MongoDB repositories."""
from pathlib import Path
import os
import re
import sys
from repo_selection import Selection


def mongodb_aliases(root):
    root = Path(root)
    base = os.environ.get('SYNORA_UPSTREAM', 'https://repo.mongodb.org').rstrip('/')
    def newest(values):
        return max(values, key=lambda v: tuple(map(int, v.split('.'))))
    def alias(parent, name, target):
        temporary = parent / ('.' + name + '.new')
        temporary.unlink(missing_ok=True)
        temporary.symlink_to(target)
        temporary.replace(parent / name)
    for family in ('ubuntu', 'debian'):
        selection = Selection('APT', base + '/apt/' + family)
        for parent in (root / 'apt' / family / 'dists').glob('*/mongodb-org'):
            versions = [p.name for p in parent.iterdir() if not p.is_symlink()
                        and re.fullmatch(r'\d+\.\d+', p.name)
                        and selection.matches('VERSIONS', f'{parent.parent.name}/mongodb-org/{p.name}')
                        and any((p / name).is_file() for name in ('Release', 'InRelease'))]
            if versions:
                alias(parent, 'stable', newest(versions))
    selection = Selection('YUM', base + '/yum/redhat')
    repositories = {}
    for path in (root / 'yum').glob('el*-*'):
        match = re.fullmatch(r'el(\d+)-(\d+\.\d+)', path.name)
        if match and not path.is_symlink() and selection.allows(match[1], 'x86_64', match[2]) and (path / 'repodata/repomd.xml').is_file():
            repositories.setdefault(match[1], []).append(match[2])
    for version, products in repositories.items():
        alias(root / 'yum', 'el' + version, f'el{version}-{newest(products)}')


if __name__ == '__main__':
    mongodb_aliases(sys.argv[1])
