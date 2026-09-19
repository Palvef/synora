"""Recover MongoDB RPM repositories whose published primary index has disappeared.

Enumerate the official public S3 bucket, verify every RPM, then let createrepo
publish an index of those packages. No upstream metadata error is ignored.
"""
import hashlib
import logging
import os
from concurrent.futures import ThreadPoolExecutor
import re
import subprocess
import xml.etree.ElementTree as ET
from pathlib import Path, PurePosixPath
from urllib.parse import quote, urlsplit
import requests

TIMEOUT = (30, 60)
BUCKET = 'https://s3.amazonaws.com/repo.mongodb.org'


def safe_relative(name):
    path = PurePosixPath(name)
    if not name or path.is_absolute() or '..' in path.parts or '\\' in name:
        raise RuntimeError('Unsafe MongoDB object path')
    return path


def needs_repair(url):
    if urlsplit(url).hostname != 'repo.mongodb.org':
        return False
    response = requests.get(url.rstrip('/')+'/repodata/repomd.xml', timeout=TIMEOUT)
    response.raise_for_status()
    primary = ET.fromstring(response.content).find("{*}data[@type='primary']/{*}location")
    if primary is None:
        raise RuntimeError('MongoDB repomd.xml has no primary metadata')
    name = str(safe_relative(primary.attrib['href']))
    response = requests.get(url.rstrip('/')+'/'+name, timeout=TIMEOUT, stream=True)
    try:
        if response.status_code in (404, 410):
            logging.warning('Official MongoDB primary metadata is missing: %s/%s; rebuilding from RPM inventory', url, name)
            return True
        response.raise_for_status()
        return False
    finally:
        response.close()


def inventory(url):
    if urlsplit(url).hostname != 'repo.mongodb.org':
        raise RuntimeError('RPM recovery is restricted to the official MongoDB bucket')
    prefix = urlsplit(url).path.strip('/')+'/'
    token = None
    seen = set()
    objects = {}
    while True:
        params = {'list-type': '2', 'prefix': prefix, 'max-keys': '1000'}
        if token:
            params['continuation-token'] = token
        response = requests.get(BUCKET, params=params, timeout=TIMEOUT)
        response.raise_for_status()
        root = ET.fromstring(response.content)
        for item in root.findall('{*}Contents'):
            key = item.findtext('{*}Key') or ''
            if not key.startswith(prefix):
                raise RuntimeError('MongoDB inventory escaped repository prefix')
            name = key[len(prefix):]
            if not name.endswith('.rpm'):
                continue
            safe_relative(name)
            size = int(item.findtext('{*}Size'))
            etag = (item.findtext('{*}ETag') or '').strip('"')
            if size <= 0:
                raise RuntimeError('Empty RPM object in MongoDB inventory')
            objects[name] = (size, etag)
        truncated = root.findtext('{*}IsTruncated')
        if truncated == 'false':
            break
        token = root.findtext('{*}NextContinuationToken')
        if truncated != 'true' or not token or token in seen:
            raise RuntimeError('Incomplete MongoDB S3 inventory; refusing metadata publication')
        seen.add(token)
    if not objects:
        raise RuntimeError('No RPMs in official MongoDB inventory; refusing empty metadata')
    return objects


def md5(path):
    digest = hashlib.md5(usedforsecurity=False)
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024*1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def download(url, path, size, etag):
    # A single-part S3 ETag is a content MD5, not a signature. RPM's embedded
    # digests are additionally verified below, including multipart objects.
    content_md5 = bool(re.fullmatch(r'[0-9a-f]{32}', etag))
    if path.is_file() and path.stat().st_size == size and content_md5 and md5(path) == etag:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name('.'+path.name+'.synora-part')
    try:
        with requests.get(url, stream=True, timeout=TIMEOUT) as response:
            response.raise_for_status()
            with temporary.open('wb') as output:
                for chunk in response.iter_content(1024*1024):
                    if chunk:
                        output.write(chunk)
        if temporary.stat().st_size != size or (content_md5 and md5(temporary) != etag):
            raise RuntimeError('MongoDB RPM size/checksum mismatch: '+url)
        subprocess.run(['rpm', '--checksig', '--nosignature', str(temporary)], check=True,
                       stdout=subprocess.DEVNULL)
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def recover(url, destination):
    from package_policy import excluded_file
    objects = {name: value for name, value in inventory(url).items() if not excluded_file(name)}
    logging.warning('Recovering %s from %d official RPM objects; removing packages absent from complete inventory', url, len(objects))
    def fetch(item):
        name, (size, etag) = item
        target = (destination/name).resolve()
        if not target.is_relative_to(destination.resolve()):
            raise RuntimeError('Local MongoDB RPM path escapes repository')
        download(url.rstrip('/')+'/'+quote(name, safe='/'), target, size, etag)
    threads = max(1, min(16, int(os.getenv('MONGO_RPM_THREADS', '4'))))
    with ThreadPoolExecutor(max_workers=threads) as executor:
        list(executor.map(fetch, objects.items()))
    # Full inventory + successful payload validation is required before pruning.
    for path in destination.rglob('*.rpm'):
        if str(path.relative_to(destination)) not in objects:
            if not path.resolve().is_relative_to(destination.resolve()):
                raise RuntimeError('MongoDB cleanup path escapes repository')
            path.unlink()
    # The caller invokes createrepo only after every package passed validation.
