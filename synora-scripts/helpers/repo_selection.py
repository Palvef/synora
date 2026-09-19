"""Shared APT/YUM include/exclude policy; empty configuration selects everything."""
import fnmatch
import os

class Selection:
    def __init__(self):
        self.rules = {}
        for kind in ('VERSIONS', 'ARCHES', 'COMPONENTS'):
            for prefix in ('SYNC_', 'SYNC_EXCLUDE_'):
                raw = os.environ.get(prefix + kind, '')
                self.rules[prefix + kind] = [v.strip() for v in raw.split(',') if v.strip()]
        self.active = any(self.rules.values())

    def matches(self, kind, value):
        include = self.rules['SYNC_' + kind]
        exclude = self.rules['SYNC_EXCLUDE_' + kind]
        return (not include or any(fnmatch.fnmatchcase(value, p) for p in include)) and not any(fnmatch.fnmatchcase(value, p) for p in exclude)

    def allows(self, version, arch, component):
        return all(self.matches(k, v) for k, v in [('VERSIONS', version), ('ARCHES', arch), ('COMPONENTS', component)])
