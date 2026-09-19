"""Shared APT/YUM include/exclude policy; empty configuration selects everything."""
import fnmatch
import json
import os
import re
from urllib.parse import urlsplit


def architecture_override(protocol, base_url):
    """Resolve an explicit job override, with exact URL-path scope taking priority."""
    key = f'SYNC_{protocol}_ARCHES'
    raw = os.environ.get(key)
    scoped = os.environ.get(key + '_BY_PATH')

    def validate(value):
        if not isinstance(value, str):
            raise ValueError(f'{key} must contain comma-separated architectures')
        arches = [arch.strip() for arch in value.split(',')]
        if any(arch != '@auto' and not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]*', arch) for arch in arches):
            raise ValueError(f'Invalid architectures in {key}')
        return ','.join(dict.fromkeys(arches))

    if raw is not None:
        raw = validate(raw)
    if scoped is not None:
        mapping = json.loads(scoped)
        if not isinstance(mapping, dict):
            raise ValueError(f'{key}_BY_PATH must be a JSON object')
        normalized = {}
        for path, value in mapping.items():
            if not path.startswith('/'):
                raise ValueError(f'{key}_BY_PATH keys must be absolute URL paths')
            path = path.rstrip('/') or '/'
            if path in normalized:
                raise ValueError(f'Duplicate URL path in {key}_BY_PATH')
            normalized[path] = validate(value)
        raw = normalized.get(urlsplit(base_url).path.rstrip('/') or '/', raw)
    return raw

class Selection:
    def __init__(self, protocol=None, base_url=''):
        self.architectures = architecture_override(protocol, base_url) if protocol else None
        self.rules = {}
        for kind in ('VERSIONS', 'ARCHES', 'COMPONENTS'):
            for prefix in ('SYNC_', 'SYNC_EXCLUDE_'):
                raw = os.environ.get(prefix + kind, '')
                self.rules[prefix + kind] = [v.strip() for v in raw.split(',') if v.strip()]
        self.active = any(self.rules.values()) or self.architectures is not None

    def matches(self, kind, value):
        include = self.rules['SYNC_' + kind]
        exclude = self.rules['SYNC_EXCLUDE_' + kind]
        return (not include or any(fnmatch.fnmatchcase(value, p) for p in include)) and not any(fnmatch.fnmatchcase(value, p) for p in exclude)

    def allows(self, version, arch, component):
        return all(self.matches(k, v) for k, v in [('VERSIONS', version), ('ARCHES', arch), ('COMPONENTS', component)])
