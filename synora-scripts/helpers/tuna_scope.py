"""Read TUNA's published selection as data, never execute downloaded sources."""
import argparse
import ast
import json
import os
from pathlib import Path
import re
import shlex
from urllib.parse import urlsplit

import requests


def templates(source):
    try:
        nodes = [n.value for n in ast.parse(source).body if isinstance(n, ast.Assign)
                 and any(isinstance(t, ast.Name) and t.id == 'OS_TEMPLATE' for t in n.targets)]
        if len(nodes) != 1:
            raise ValueError('Expected one OS_TEMPLATE')
        result = ast.literal_eval(nodes[0])
        if not isinstance(result, dict) or not result:
            raise ValueError('Empty OS_TEMPLATE')
        for values in result.values():
            if not isinstance(values, list) or not values or any(not isinstance(v, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*', v) for v in values):
                raise ValueError('Invalid OS_TEMPLATE values')
        return result
    except (SyntaxError, TypeError, KeyError) as e:
        raise ValueError('Invalid OS_TEMPLATE') from e


def calls(source, command):
    result = []
    for line in source.splitlines():
        if line.strip().startswith('"$' + command + '"'):
            args = shlex.split(line)
            result.append([a for a in args[1:] if a != '--delete'])
    return result


def parse_scope(repository, source, apt_source, yum_source, commit):
    apt = templates(apt_source)
    rules = []
    def rule(protocol, path, versions, components, arches):
        rules.append(dict(protocol=protocol, path=path, versions=versions,
                          components=components, arches=arches.split(',')))
    data = dict(schema=1, repository=repository, source_commit=commit, rules=rules)
    try:
        if repository == 'mongodb':
            match = re.search(r'^MONGO_VERSIONS=\(([^\n]*)\)$', source, re.M)
            stable = re.search(r'^STABLE_VERSION="(\d+\.\d+)"$', source, re.M)
            if not match or not stable:
                raise ValueError('Unrecognized MongoDB version declarations')
            versions = shlex.split(match[1])
            if not versions or any(not re.fullmatch(r'\d+\.\d+', v) for v in versions) or stable[1] not in versions:
                raise ValueError('Invalid MongoDB formal versions')
            data['stable_version'] = stable[1]
            rpm = calls(source, 'yum_sync')
            if len(rpm) != 1 or len(rpm[0]) != 6 or rpm[0][1:3] != ['@rhel-current', '$components']:
                raise ValueError('Unrecognized MongoDB RPM invocation')
            if rpm[0][0] != '${BASE_URL}/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}/':
                raise ValueError('Unrecognized MongoDB RPM path')
            rule('YUM', '/yum/redhat', templates(yum_source)['rhel-current'], versions, rpm[0][3])
            invocations = calls(source, 'apt_sync')
            if len(invocations) != 2:
                raise ValueError('Expected two MongoDB APT invocations')
            for family, alias in [('ubuntu', 'ubuntu-lts'), ('debian', 'debian-current')]:
                rows = [r for r in invocations if r[0] == '$BASE_URL/apt/' + family]
                if len(rows) != 1 or len(rows[0]) != 5 or rows[0][1] != '${components:1}':
                    raise ValueError('Unrecognized MongoDB APT invocation')
                suites = [f'{dist}/mongodb-org/{v}' for dist in apt[alias] for v in versions]
                rule('APT', '/apt/' + family, suites, rows[0][2].split(','), rows[0][3])
        elif repository == 'proxmox':
            rows = calls(source, 'apt_sync')
            if len(rows) != 4:
                raise ValueError('Expected four Proxmox APT invocations')
            for row in rows:
                if len(row) != 5 or row[1] != '@debian-current' or not row[0].startswith('${BASE_URL}/debian/'):
                    raise ValueError('Unrecognized Proxmox APT invocation')
                rule('APT', row[0].removeprefix('${BASE_URL}'), apt['debian-current'], row[2].split(','), row[3])
        else:
            raise ValueError('Unsupported TUNA scope repository')
    except (KeyError, IndexError) as e:
        raise ValueError('Missing TUNA selection data') from e
    validate(data)
    return data


def validate(data):
    expected = {'mongodb': {('APT', '/apt/ubuntu'), ('APT', '/apt/debian'), ('YUM', '/yum/redhat')},
                'proxmox': {('APT', '/debian/' + r) for r in ('pve', 'pbs', 'pbs-client', 'pmg')}}
    if not isinstance(data, dict) or data.get('schema') != 1 or data.get('repository') not in expected or not re.fullmatch(r'[0-9a-f]{40}', data.get('source_commit', '')):
        raise ValueError('Invalid TUNA scope header')
    rules = data.get('rules', [])
    if len(rules) != len(expected[data['repository']]) or {(r.get('protocol'), r.get('path')) for r in rules} != expected[data['repository']]:
        raise ValueError('Invalid TUNA scope paths')
    for r in rules:
        for key in ('versions', 'components', 'arches'):
            values = r.get(key)
            if not isinstance(values, list) or not values or any(not isinstance(v, str) or any(not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*', part) for part in v.split('/')) for v in values):
                raise ValueError('Invalid TUNA scope values')
    if data['repository'] == 'mongodb':
        rpm = next(r for r in rules if r['protocol'] == 'YUM')
        if data.get('stable_version') not in rpm['components']:
            raise ValueError('Invalid TUNA stable version')
    return data


def fetch_scope(repository):
    def fetch(url):
        proxy = os.environ.get('TUNA_SCOPE_PROXY')
        options = {'proxies': {'http': proxy, 'https': proxy}} if proxy else {}
        response = requests.get(url, timeout=(15, 60), headers={'User-Agent': 'Synora-scope/1.0'}, **options)
        response.raise_for_status()
        return response
    commit = fetch('https://api.github.com/repos/tuna/tunasync-scripts/commits/master').json()['sha']
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('Invalid TUNA commit')
    base = f'https://raw.githubusercontent.com/tuna/tunasync-scripts/{commit}/'
    return parse_scope(repository, fetch(base + repository + '.sh').text,
                       fetch(base + 'apt-sync.py').text, fetch(base + 'yum-sync.py').text, commit)


def configured_rule(protocol, base_url):
    policy = os.environ.get('SYNC_SCOPE_POLICY', '')
    if not policy:
        return None
    if policy != 'tuna' or not os.environ.get('SYNORA_TUNA_SCOPE_FILE'):
        raise ValueError('SYNC_SCOPE_POLICY=tuna requires a prepared scope file')
    data = validate(json.loads(Path(os.environ['SYNORA_TUNA_SCOPE_FILE']).read_text()))
    path = urlsplit(base_url).path.rstrip('/')
    matches = [r for r in data['rules'] if r['protocol'] == protocol and (path == r['path'] or path.startswith(r['path'] + '/'))]
    if len(matches) != 1:
        raise ValueError('URL does not match the prepared TUNA scope')
    return matches[0]


def mongodb_aliases(root, scope=None):
    root = Path(root)
    if scope is not None:
        validate(scope)
    def alias(parent, name, target):
        temporary = parent / ('.' + name + '.new')
        temporary.unlink(missing_ok=True)
        temporary.symlink_to(target)
        temporary.replace(parent / name)
    for family in ('ubuntu', 'debian'):
        rule = next((r for r in scope['rules'] if r['path'] == '/apt/' + family), None) if scope else None
        for parent in (root / 'apt' / family / 'dists').glob('*/mongodb-org'):
            versions = [p.name for p in parent.iterdir() if not p.is_symlink() and re.fullmatch(r'\d+\.\d+', p.name) and any((p / f).is_file() for f in ('Release', 'InRelease'))]
            if rule:
                versions = [v for v in versions if f'{parent.parent.name}/mongodb-org/{v}' in rule['versions'] and v == scope['stable_version']]
            if versions:
                alias(parent, 'stable', max(versions, key=lambda v: tuple(map(int, v.split('.')))))
    if scope:
        rule = next(r for r in scope['rules'] if r['protocol'] == 'YUM')
        for version in rule['versions']:
            target = f"el{version}-{scope['stable_version']}"
            if (root / 'yum' / target / 'repodata/repomd.xml').is_file():
                alias(root / 'yum', 'el' + version, target)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['prepare', 'mongodb-aliases'])
    parser.add_argument('target')
    args = parser.parse_args()
    if args.action == 'prepare':
        data = fetch_scope(args.target)
        print(json.dumps(data, sort_keys=True))
    else:
        scope = None
        if os.environ.get('SYNC_SCOPE_POLICY'):
            configured_rule('YUM', 'https://repo.mongodb.org/yum/redhat')
            scope = json.loads(Path(os.environ['SYNORA_TUNA_SCOPE_FILE']).read_text())
        mongodb_aliases(args.target, scope)


if __name__ == '__main__':
    main()
