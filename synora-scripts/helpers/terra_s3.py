"""Unsigned S3 listing and exact-object mirroring through an explicit proxy."""
from concurrent.futures import ThreadPoolExecutor
from hashlib import md5
import json
import logging
import os
from pathlib import Path
import re
import tempfile
from urllib.parse import quote, urlsplit
import xml.etree.ElementTree as ET

import requests
from requests.adapters import HTTPAdapter
from urllib3.util.retry import Retry

NS = {'s': 'http://s3.amazonaws.com/doc/2006-03-01/'}


def session():
    proxy = next((os.environ[key] for key in ('HTTPS_PROXY', 'https_proxy', 'ALL_PROXY', 'all_proxy', 'HTTP_PROXY', 'http_proxy') if os.environ.get(key)), '')
    if urlsplit(proxy).scheme not in ('http', 'https', 'socks5', 'socks5h'):
        raise ValueError('Terra S3 sync requires an explicit proxy; direct fallback is disabled')
    client = requests.Session()
    client.trust_env = False
    client.proxies = {'http': proxy, 'https': proxy}
    client.mount('https://', HTTPAdapter(max_retries=Retry(total=3, backoff_factor=1, status_forcelist=[429, 500, 502, 503, 504])))
    return client


def objects(base, repository):
    marker, seen, result = '', set(), {}
    with session() as client:
        for _ in range(10000):
            response = client.get(base, params={'prefix': repository + '/', 'marker': marker, 'max-keys': 1000}, timeout=(30, 120))
            response.raise_for_status()
            root = ET.fromstring(response.content)
            if root.tag != '{http://s3.amazonaws.com/doc/2006-03-01/}ListBucketResult':
                raise ValueError('Invalid S3 object listing')
            for item in root.findall('s:Contents', NS):
                key = item.findtext('s:Key', namespaces=NS) or ''
                if not key.startswith(repository + '/') or any(part in ('', '.', '..') for part in key.rstrip('/').split('/')) or '\\' in key:
                    raise ValueError('Unsafe S3 object key')
                if key.endswith('/'):
                    continue
                relative = key[len(repository) + 1:]
                result[relative] = {'size': int(item.findtext('s:Size', namespaces=NS)), 'etag': item.findtext('s:ETag', namespaces=NS).strip('"')}
            if root.findtext('s:IsTruncated', namespaces=NS) == 'false':
                if 'repodata/repomd.xml' not in result:
                    raise ValueError(f'{repository}: listing has no repomd.xml; refusing deletion')
                return result
            marker = root.findtext('s:NextMarker', namespaces=NS)
            if not marker or marker in seen:
                raise ValueError('Invalid S3 object pagination')
            seen.add(marker)
    raise ValueError('S3 object listing exceeded pagination limit')


def download(base, repository, relative, info, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if any(parent.is_symlink() for parent in [destination, *destination.parents]):
        raise ValueError('Refusing symlink in mirror destination')
    temporary = None
    try:
        with session() as client, client.get(f'{base}/{repository}/{quote(relative, safe="/")}', headers={'If-Match': '"' + info['etag'] + '"'}, timeout=(30, 120), stream=True) as response:
            response.raise_for_status()
            digest, size = md5(usedforsecurity=False), 0
            with tempfile.NamedTemporaryFile(dir=destination.parent, prefix='.terra-', delete=False) as output:
                temporary = Path(output.name)
                for chunk in response.iter_content(1024 * 1024):
                    output.write(chunk)
                    digest.update(chunk)
                    size += len(chunk)
            if size != info['size'] or (re.fullmatch('[0-9a-fA-F]{32}', info['etag']) and digest.hexdigest().lower() != info['etag'].lower()):
                raise ValueError(f'{repository}/{relative}: object size or ETag mismatch')
            temporary.chmod(0o644)
            temporary.replace(destination)
            logging.info('downloaded %s/%s (%s bytes)', repository, relative, size)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def sync(base, repository, storage):
    listing = objects(base, repository)
    target = storage / repository
    if target.is_symlink():
        raise ValueError('Refusing repository symlink')
    target.mkdir(parents=True, exist_ok=True)
    state_dir = storage / '.synora' / 'terra'
    state_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / (repository + '.json')
    previous = json.loads(state_file.read_text()) if state_file.exists() else {}
    def cached(path, info):
        destination = target / path
        if not destination.is_file() or destination.is_symlink() or destination.stat().st_size != info['size']:
            return False
        if previous.get(path) == info:
            return True
        if re.fullmatch('[0-9a-fA-F]{32}', info['etag']):
            digest = md5(usedforsecurity=False)
            with destination.open('rb') as content:
                for chunk in iter(lambda: content.read(1024 * 1024), b''):
                    digest.update(chunk)
            return digest.hexdigest().lower() == info['etag'].lower()
        return False

    pending = [(path, info) for path, info in listing.items() if not cached(path, info)]
    packages = [(path, info) for path, info in pending if not path.startswith('repodata/')]
    metadata = [(path, info) for path, info in pending if path.startswith('repodata/')]
    logging.info('%s: %s objects, %s downloads (no filtering)', repository, len(listing), len(pending))
    with ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(lambda item: download(base, repository, item[0], item[1], target / item[0]), packages))
    # Stage all metadata until payloads and metadata are completely downloaded.
    with tempfile.TemporaryDirectory(prefix='.terra-metadata-', dir=target) as temporary:
        stage = Path(temporary)
        for path, info in metadata:
            download(base, repository, path, info, stage / path)
        for path, _ in sorted(metadata, key=lambda item: item[0] == 'repodata/repomd.xml'):
            destination = target / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            if any(parent.is_symlink() for parent in [destination, *destination.parents]):
                raise ValueError('Refusing metadata symlink')
            (stage / path).replace(destination)
    removed = 0
    paths = list(target.rglob('*'))
    if any(path.is_symlink() for path in paths):
        raise ValueError('Refusing symlink during mirror cleanup')
    for path in paths:
        if path.is_file() and str(path.relative_to(target)) not in listing:
            path.unlink()
            removed += 1
    for path in sorted(target.rglob('*'), key=lambda p: len(p.parts), reverse=True):
        if path.is_dir() and not any(path.iterdir()):
            path.rmdir()
    temporary = state_file.with_suffix('.tmp')
    temporary.write_text(json.dumps(listing, sort_keys=True))
    temporary.replace(state_file)
    logging.info('%s complete: %s objects, %s obsolete files removed', repository, len(listing), removed)
