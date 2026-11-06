#!/usr/bin/env python3
"""Preserve Android's tag-valued mirror heads without changing their object IDs."""
import os
import subprocess
import sys
import time

PREFIX = 'refs/synora/aosp-heads/'
REFSPEC = '+refs/heads/*:refs/heads/*'


def fetch_with_retry(git, arguments, attempts=5, delay=2):
    transient = (b'CONNECT tunnel failed, response 502',
                 b'Proxy CONNECT aborted', b'Connection timed out',
                 b'Failed to connect', b'Could not resolve proxy',
                 b'Could not resolve host')
    for attempt in range(1, attempts + 1):
        tail = b''
        with subprocess.Popen([git, *arguments], stderr=subprocess.PIPE) as process:
            while chunk := process.stderr.read(8192):
                sys.stderr.write(chunk.decode('utf-8', errors='replace'))
                sys.stderr.flush()
                tail = (tail + chunk)[-65536:]
            code = process.wait()
        if code == 0 or attempt == attempts or not any(error in tail for error in transient):
            return code
        pause = delay * 2 ** (attempt - 1)
        print(f'AOSP Git transport retry {attempt}/{attempts - 1} in {pause}s: transient proxy/network failure', file=sys.stderr, flush=True)
        time.sleep(pause)


def main():
    git = os.environ['SYNORA_AOSP_REAL_GIT']
    args = sys.argv[1:]
    if not args or args[0] != 'fetch':
        os.execv(git, [git, *args])

    def run(*arguments, **kwargs):
        return subprocess.run([git, *arguments], check=True, **kwargs)

    mapped = [('+refs/heads/*:' + PREFIX + '*') if arg == REFSPEC else arg for arg in args]
    if REFSPEC not in args:
        return fetch_with_retry(git, args)
    # Explicit mapping must not also update remote.aosp.fetch's refs/heads targets.
    code = fetch_with_retry(git, [*mapped, '--refmap='])
    if code:
        return code
    rows = run('for-each-ref', '--format=%(refname) %(objectname) %(objecttype)', PREFIX, capture_output=True, text=True).stdout.splitlines()
    heads = {}
    for row in rows:
        ref, oid, kind = row.split()
        if kind != 'commit':
            # AOSP signs branch tips with annotated tags; reject other malformed heads.
            if kind != 'tag':
                raise RuntimeError(f'Unsupported upstream branch object: {ref} ({kind})')
            run('rev-parse', '--verify', oid + '^{commit}', stdout=subprocess.DEVNULL)
        heads['refs/heads/' + ref[len(PREFIX):]] = (ref, oid, kind)
    old = run('for-each-ref', '--format=%(refname)', 'refs/heads/', capture_output=True, text=True).stdout.splitlines()
    transaction = ['option no-deref']
    for ref in old:
        if ref not in heads:
            transaction.append('delete ' + ref)
    for ref, (_, oid, kind) in heads.items():
        if kind == 'commit':
            transaction.append(f'update {ref} {oid}')
    run('update-ref', '--stdin', input='\n'.join(transaction) + '\n', text=True)
    for ref, (target, _, kind) in heads.items():
        if kind == 'tag':
            run('symbolic-ref', ref, target)
    hidden = subprocess.run([git, 'config', '--get-all', 'transfer.hideRefs'], capture_output=True, text=True).stdout.splitlines()
    if PREFIX not in hidden:
        run('config', '--add', 'transfer.hideRefs', PREFIX)
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (subprocess.CalledProcessError, RuntimeError) as error:
        print(f'AOSP mirror ref update failed: {error}', file=sys.stderr)
        sys.exit(1)
