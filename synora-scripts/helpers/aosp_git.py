#!/usr/bin/env python3
"""Preserve Android's tag-valued mirror heads without changing their object IDs."""
import os
import subprocess
import sys

PREFIX = 'refs/synora/aosp-heads/'
REFSPEC = '+refs/heads/*:refs/heads/*'


def main():
    git = os.environ['SYNORA_AOSP_REAL_GIT']
    args = sys.argv[1:]
    if not args or args[0] != 'fetch' or REFSPEC not in args:
        os.execv(git, [git, *args])

    def run(*arguments, **kwargs):
        return subprocess.run([git, *arguments], check=True, **kwargs)

    mapped = [('+refs/heads/*:' + PREFIX + '*') if arg == REFSPEC else arg for arg in args]
    # Explicit mapping must not also update remote.aosp.fetch's refs/heads targets.
    result = subprocess.run([git, *mapped, '--refmap='])
    if result.returncode:
        return result.returncode
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
