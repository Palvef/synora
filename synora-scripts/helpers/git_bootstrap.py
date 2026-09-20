#!/usr/bin/env python3
"""Opt-in static Git seed, fetched directly before the official remote update."""
from pathlib import Path
import re
import http.client
import subprocess
import sys
import time
import urllib.request


def pack_names(text):
    names = []
    for line in text.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r'P (pack-[0-9a-f]{40}(?:[0-9a-f]{24})?\.pack)', line.strip())
        if not match:
            raise ValueError('Invalid static Git pack manifest')
        names.append(match[1])
    if not names or len(names) > 1000:
        raise ValueError('Empty or excessive static Git pack manifest')
    return names


def download(opener, url, path):
    """Resume only when the server confirms the requested byte offset."""
    for attempt in range(1, 4):
        offset = path.stat().st_size if path.exists() else 0
        headers = {'Range': f'bytes={offset}-'} if offset else {}
        try:
            with opener.open(urllib.request.Request(url, headers=headers), timeout=60) as response:
                append = response.status == 206 and offset > 0
                if response.status == 206 and not response.headers.get('Content-Range', '').startswith(f'bytes {offset}-'):
                    raise ValueError('Incorrect resumed byte range')
                if offset and not append:
                    print('Seed server does not support resume; restarting incomplete pack', flush=True)
                expected = response.headers.get('Content-Length')
                received = 0
                reported = time.monotonic()
                with path.open('ab' if append else 'wb') as output:
                    while chunk := response.read(1024 * 1024):
                        output.write(chunk)
                        received += len(chunk)
                        if time.monotonic() - reported >= 30:
                            print(f'Seed progress: {path.name}: {output.tell()} bytes', flush=True)
                            reported = time.monotonic()
                if expected is not None and received != int(expected):
                    raise OSError('Incomplete static pack response')
                return
        except (OSError, ValueError, http.client.HTTPException) as error:
            if attempt == 3:
                raise
            print(f'Seed transfer interrupted ({type(error).__name__}); retry {attempt}/2', flush=True)
            time.sleep(5 * attempt)


def bootstrap(url, repo):
    if not url.startswith(('https://', 'http://')):
        raise ValueError('Bootstrap URL must use HTTP(S)')
    repo = Path(repo).resolve()
    # Bootstrap is deliberately direct; the configured job proxy remains in
    # effect for the subsequent fetch from the official upstream.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    base = url.rstrip('/') + '/'
    def metadata(path):
        with opener.open(base + path, timeout=60) as response:
            data = response.read(16 * 1024 * 1024 + 1)
            if len(data) > 16 * 1024 * 1024:
                raise ValueError('Excessive Git seed metadata')
            return data.decode('ascii')
    head = metadata('HEAD').strip()
    if not head.startswith('ref: refs/heads/'):
        raise ValueError('Seed HEAD is not a branch')
    branch = head.removeprefix('ref: ')
    subprocess.run(['git', 'check-ref-format', branch], check=True)
    refs = [line.split() for line in metadata('info/refs').splitlines()]
    heads = [fields[0] for fields in refs if len(fields) == 2 and fields[1] == branch]
    if len(heads) != 1 or not re.fullmatch(r'[0-9a-f]{40}(?:[0-9a-f]{24})?', heads[0]):
        raise ValueError('Seed default branch is missing or invalid')
    names = pack_names(metadata('objects/info/packs'))
    staging = repo / '.synora-bootstrap'
    staging.mkdir(exist_ok=True)
    packs = repo / 'objects/pack'
    packs.mkdir(parents=True, exist_ok=True)
    for name in names:
        target = packs / name
        index = target.with_suffix('.idx')
        if target.exists() and index.exists():
            continue
        partial = staging / name
        print(f'Seeding Git pack {name}', flush=True)
        download(opener, base + 'objects/pack/' + name, partial)
        try:
            digest = subprocess.check_output(['git', 'index-pack', str(partial)], text=True).strip()
            if digest != name.removeprefix('pack-').removesuffix('.pack'):
                raise ValueError('Seed pack checksum differs from manifest')
        except (subprocess.CalledProcessError, ValueError):
            partial.unlink(missing_ok=True)
            partial.with_suffix('.idx').unlink(missing_ok=True)
            raise
        partial.replace(target)
        partial.with_suffix('.idx').replace(index)
    subprocess.run(['git', '-C', str(repo), 'fsck', '--full', '--no-dangling', heads[0]], check=True)
    subprocess.run(['git', '-C', str(repo), 'update-ref', 'refs/synora-bootstrap/seed', heads[0]], check=True)
    print('Seed connectivity verified; fetching official upstream before reporting success', flush=True)


if __name__ == '__main__':
    bootstrap(sys.argv[1], Path(sys.argv[2]))
