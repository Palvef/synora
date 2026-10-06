#!/usr/bin/env python3
"""Discover Fedora Terra repositories from the upstream S3 listing."""
import argparse
import logging
import os
from pathlib import Path
import re
import shutil
import sys
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / 'helpers'))
from repo_selection import Selection
from terra_s3 import sync, session

NAMESPACE = {'s': 'http://s3.amazonaws.com/doc/2006-03-01/'}
REPOSITORY = re.compile(r'terra([0-9]+|rawhide)(?:-(extras|mesa|multimedia|nvidia))?/$')


def discover(base_url):
    repositories = {}
    marker = ''
    seen = set()
    with session() as client:
        for _ in range(100):
            response = client.get(base_url, params={'delimiter': '/', 'marker': marker}, timeout=(30, 60))
            response.raise_for_status()
            root = ET.fromstring(response.content)
            if root.tag != '{http://s3.amazonaws.com/doc/2006-03-01/}ListBucketResult':
                raise ValueError('Invalid Terra S3 repository listing')
            for prefix in root.findall('s:CommonPrefixes/s:Prefix', NAMESPACE):
                match = REPOSITORY.fullmatch(prefix.text or '')
                if match:
                    repositories[prefix.text.rstrip('/')] = (match[1], match[2] or 'main')
            if root.findtext('s:IsTruncated', namespaces=NAMESPACE) == 'false':
                if not repositories:
                    raise ValueError('No Fedora Terra repositories discovered')
                return sorted(repositories.items())
            marker = root.findtext('s:NextMarker', namespaces=NAMESPACE)
            if not marker or marker in seen:
                raise ValueError('Invalid Terra listing pagination')
            seen.add(marker)
        raise ValueError('Terra listing exceeded pagination limit')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s: %(message)s')
    os.environ['SYNORA_REPOSITORY'] = 'terra'
    base = os.environ.get('SYNORA_UPSTREAM', 'https://repos.fyralabs.com').rstrip('/')
    repositories = discover(base)
    selection = Selection('YUM', base)
    selected = [(name, version, component) for name, (version, component) in repositories
                if selection.matches('VERSIONS', version) and selection.matches('COMPONENTS', component)]
    if not selected:
        raise ValueError('No Terra repositories selected')
    if args.dry_run:
        for name, version, component in selected:
            print(f'{name}: version={version} component={component} url={base}/{name}')
        return
    storage = Path(os.environ['SYNORA_STORAGE'])
    storage.mkdir(parents=True, exist_ok=True)
    for name, version, component in selected:
        logging.info('Syncing %s unchanged through S3 and the configured proxy', name)
        sync(base, name, storage)
    advertised = {name for name, _ in repositories}
    for path in storage.iterdir():
        if path.is_dir() and not path.is_symlink() and REPOSITORY.fullmatch(path.name + '/') and path.name not in advertised:
            shutil.rmtree(path)


if __name__ == '__main__':
    main()
