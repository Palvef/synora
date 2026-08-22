#!/bin/bash
# kali uses the same ftpsync wrapper as debian.
set -euo pipefail
exec "$(dirname "$(realpath "$0")")/debian.sh" "$@"
