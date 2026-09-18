#!/usr/bin/env python3
"""Promote a completed PyPI bootstrap after private HTTP and pip checks.

Run as a Synora success hook on the mirror web host. Until the success marker exists,
this does nothing. All modified configuration is backed up before promotion.
"""
import datetime
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile

INCLUDE = 'include /etc/nginx/snippets/synora-pypi-server.conf;'


def local_routes(text):
    if INCLUDE in text:
        return text
    for location in ('/pypi/', '@pypi_404'):
        match = re.search(r'(?m)^\s*location\s+' + re.escape(location) + r'\s*\{', text)
        if not match:
            raise ValueError('expected old PyPI location not found: ' + location)
        depth = 1
        end = match.end()
        while depth and end < len(text):
            depth += (text[end] == '{') - (text[end] == '}')
            end += 1
        if depth:
            raise ValueError('unbalanced old PyPI location')
        text = text[:match.start()] + ('\n    ' + INCLUDE if location == '/pypi/' else '') + text[end:]
    return text


def atomic_write(path, content):
    path = path.resolve()
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as handle:
        tmp = Path(handle.name)
        handle.write(content)
        handle.flush()
        os.fsync(handle.fileno())
    if path.exists():
        shutil.copystat(path, tmp)
    else:
        tmp.chmod(0o644)
    tmp.replace(path)


def run(*args, timeout=300):
    subprocess.run(args, check=True, timeout=timeout)


def main():
    state = Path('/data/pypi/.synora')
    if not (state / 'initial-success.json').is_file():
        print('PyPI bootstrap is still running; keeping existing public routes.', flush=True)
        return
    if (state / 'activated.json').exists():
        return
    with Path('/run/synora-pypi-activate.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if (state / 'activated.json').exists():
            return
        # Synora hooks have a 30-second command timeout. Stop promotion early
        # enough to restore both site configs before that outer deadline.
        def deadline(_signum, _frame):
            raise TimeoutError('activation exceeded 22s; retry on next successful Synora run')
        signal.signal(signal.SIGALRM, deadline)
        signal.alarm(22)
        base = Path(__file__).resolve().parent
        run('python3', str(base / 'nginx/verify_nginx.py'), '--base-url',
            'http://127.0.0.1:18081', '--storage', '/data/pypi')
        # Exercise the actual pip client without installing anything on the host.
        job = Path('/etc/synora/jobs/pypi.toml')
        job_before = job.read_text()
        image = re.search(r'(?m)^image\s*=\s*"([^"]+)"', job_before).group(1)
        run('docker', 'run', '--rm', '--network', 'host', '--cap-drop=ALL',
            '--security-opt=no-new-privileges', '--entrypoint', 'python', image,
            '-m', 'pip', 'download', '--no-deps', '--disable-pip-version-check',
            '--index-url', 'http://127.0.0.1:18081/pypi/simple/',
            '--trusted-host', '127.0.0.1', '--timeout', '5', '--retries', '0',
            '--dest', '/tmp/pypi-acceptance', 'pip', timeout=12)
        paths = [Path('/etc/nginx/sites-enabled') / domain
                 for domain in ('mirror.nyist.edu.cn', 'mirrors.ha.edu.cn')]
        originals = {p: p.read_text() for p in paths}
        replacements = {p: local_routes(value) for p, value in originals.items()}
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
        backup = Path('/root') / ('synora-pypi-activation-' + stamp)
        backup.mkdir(mode=0o700)
        for p, content in originals.items():
            (backup / p.name).write_text(content)
        (backup / job.name).write_text(job_before)
        try:
            for p, content in replacements.items():
                atomic_write(p, content)
            run('nginx', '-t', timeout=3)
            run('systemctl', 'reload', 'nginx', timeout=3)
            for host in ('mirror.nyist.edu.cn', 'mirrors.ha.edu.cn'):
                run('python3', str(base / 'nginx/verify_nginx.py'), '--base-url',
                    'http://172.31.32.150', '--host', host, '--storage', '/data/pypi')
            atomic_write(state / 'activated.json', json.dumps({
                'activated_at': stamp, 'backup': str(backup), 'image': image,
                'interval': '5m',
            }) + '\n')
            print('PyPI local serving enabled on both sites; five-minute sync enabled.', flush=True)
        except BaseException:
            signal.alarm(0)
            for p, content in originals.items():
                atomic_write(p, content)
            run('nginx', '-t', timeout=3)
            run('systemctl', 'reload', 'nginx', timeout=3)
            raise
        finally:
            signal.alarm(0)


if __name__ == '__main__':
    main()
