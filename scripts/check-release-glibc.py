#!/usr/bin/env python3
"""Reject release executables requiring a newer glibc than Ubuntu 22.04."""

import re
import subprocess
import sys

LIMIT = (2, 35)


def main():
    if len(sys.argv) < 2:
        raise SystemExit("usage: check-release-glibc.py EXECUTABLE [EXECUTABLE ...]")
    failed = False
    for binary in sys.argv[1:]:
        output = subprocess.check_output(
            ["readelf", "--version-info", "--wide", "--", binary], text=True
        )
        versions = {
            tuple(map(int, version.split(".")))
            for version in re.findall(r"GLIBC_([0-9.]+)", output)
        }
        if not versions:
            print(f"{binary}: no GLIBC symbol requirements found", file=sys.stderr)
            failed = True
            continue
        required = max(versions)
        compatible = required <= LIMIT
        print(f"{binary}: GLIBC_{'.'.join(map(str, required))} "
              f"({'OK' if compatible else 'exceeds GLIBC_2.35'})")
        failed |= not compatible
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
