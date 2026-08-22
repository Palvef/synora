#!/bin/bash
# Syntax-check every Synora mirror script.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")" && pwd)
cd "$ROOT"
fail=0
if grep -R -n --exclude=README.md --exclude=check.sh 'TUNASYNC_' .; then
  echo "error: leftover TUNASYNC_ variable" >&2
  fail=1
fi
while IFS= read -r -d '' f; do
  if ! bash -n "$f"; then
    echo "bash -n failed: $f" >&2
    fail=1
  fi
done < <(find . -type f \( -name '*.sh' -o -name 'helpers' -o -name 'apt-download' -o -name 'apt-download-binary' \) -print0)
while IFS= read -r -d '' f; do
  if ! python3 -m py_compile "$f"; then
    echo "py_compile failed: $f" >&2
    fail=1
  fi
done < <(find . -type f -name '*.py' -print0)
rm -f ./helpers/__pycache__/*.pyc ./excludes/__pycache__/*.pyc
rmdir ./helpers/__pycache__ ./excludes/__pycache__ 2>/dev/null || true
if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "synora-scripts: ok"
