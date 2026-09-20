"""Shared repository defaults, with optional explicit groups and job overrides."""
import functools
import os
from pathlib import Path
import re
import tomllib
from urllib.parse import urlsplit

DEFAULT_FILE = Path(__file__).resolve().parents[1] / 'repository-defaults.toml'
FIELDS = {'arches', 'versions', 'components'}


def values_valid(values):
    return isinstance(values, list) and bool(values) and all(
        isinstance(v, str) and bool(re.fullmatch(r'[A-Za-z0-9@*?{}][A-Za-z0-9_./@*?{}+-]*', v))
        and all(p not in ('', '.', '..') for p in v.split('/')) for v in values)


@functools.lru_cache(maxsize=8)
def read_config(path, stamp):
    with Path(path).open('rb') as stream:
        config = tomllib.load(stream)
    if config.get('schema') != 1 or set(config) - {'schema', 'groups', 'discovery', 'defaults', 'repositories', 'packages'}:
        raise ValueError('Invalid repository defaults schema')
    packages = config.get('packages', {})
    if set(packages) - {'exclude_segments', 'exclude_suffixes'}:
        raise ValueError('Invalid package defaults')
    for key, values in packages.items():
        pattern = r'[a-z][a-z0-9]*' if key == 'exclude_segments' else r'\.[a-z0-9]+'
        if not isinstance(values, list) or any(not isinstance(v, str) or not re.fullmatch(pattern, v) for v in values):
            raise ValueError('Invalid package exclusion values')
    for name, values in config.get('groups', {}).items():
        if not re.fullmatch(r'[a-z0-9-]+', name) or not values_valid(values) or any(not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.+-]*', v) for v in values):
            raise ValueError('Invalid release group')
    for name, count in config.get('discovery', {}).items():
        if name not in {'rhel-current', 'fedora-current', 'ubuntu-lts', 'debian-current', 'debian-latest2', 'debian-latest'} or type(count) is not int or not 1 <= count <= 20:
            raise ValueError('Invalid release discovery window')
    def validate(rule, paths=False):
        if not isinstance(rule, dict) or set(rule) - (FIELDS | ({'paths'} if paths else set())):
            raise ValueError('Invalid repository defaults rule')
        for key in FIELDS & rule.keys():
            if not values_valid(rule[key]):
                raise ValueError('Empty or invalid repository default values')
        for path, scoped in rule.get('paths', {}).items():
            if not path.startswith('/') or path == '/' or path.endswith('/') or any(p in ('', '.', '..') for p in path[1:].split('/')):
                raise ValueError('Invalid repository URL path')
            validate(scoped)
    for protocol, rule in config.get('defaults', {}).items():
        if protocol not in ('APT', 'YUM'): raise ValueError('Invalid defaults protocol')
        validate(rule)
    for repo, protocols in config.get('repositories', {}).items():
        if not re.fullmatch(r'[a-z0-9-]+', repo) or not isinstance(protocols, dict):
            raise ValueError('Invalid repository name')
        for protocol, rule in protocols.items():
            if protocol not in ('APT', 'YUM'): raise ValueError('Invalid repository protocol')
            validate(rule, paths=True)
    return config


def configuration():
    path = Path(os.environ.get('SYNC_DEFAULTS_FILE', str(DEFAULT_FILE)))
    return read_config(str(path), path.stat().st_mtime_ns)


def discovery_window(name):
    try:
        return configuration()['discovery'][name]
    except KeyError as exc:
        raise ValueError('Missing release discovery group: ' + name) from exc


def fixed_group(name):
    return configuration().get('groups', {}).get(name)


def group_values(name):
    values = fixed_group(name)
    if values is not None:
        return values
    from repo_discovery import distro_versions, rpm_versions
    if name in ('rhel-current', 'fedora-current'):
        return rpm_versions('@' + name)
    if name in ('ubuntu-lts', 'debian-current', 'debian-latest2', 'debian-latest'):
        return distro_versions(name)
    raise ValueError('Unknown repository release group: ' + name)


def expand_groups(patterns):
    result = []
    for pattern in patterns:
        match = re.search(r'@\{([a-z0-9-]+)\}|^@([a-z0-9-]+)$', pattern)
        if match:
            for value in group_values(match[1] or match[2]):
                result.extend(expand_groups([pattern[:match.start()] + value + pattern[match.end():]]))
        elif '@' in pattern:
            raise ValueError('Unresolved repository release group: ' + pattern)
        else:
            result.append(pattern)
    return list(dict.fromkeys(result))


def repository_rule(protocol, base_url):
    repository = os.environ.get('SYNORA_REPOSITORY')
    if not repository:
        return {}
    config = configuration()
    if repository not in config.get('repositories', {}):
        raise ValueError('Unknown repository defaults: ' + repository)
    if protocol not in ('APT', 'YUM'):
        raise ValueError('A repository profile requires APT or YUM')
    rule = dict(config.get('defaults', {}).get(protocol, {}))
    specific = config['repositories'][repository].get(protocol, {})
    rule.update({k: v for k, v in specific.items() if k != 'paths'})
    path = urlsplit(base_url).path.rstrip('/')
    for prefix, scoped in sorted(specific.get('paths', {}).items(), key=lambda item: len(item[0])):
        if path == prefix or path.startswith(prefix + '/'):
            rule.update(scoped)
    return rule
