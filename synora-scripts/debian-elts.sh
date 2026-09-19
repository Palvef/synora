#!/bin/bash
set -e
set -o pipefail

_here=$(dirname $(realpath $0))
apt_sync="${_here}/apt-sync.py"
function join_by { local IFS="$1"; shift; echo "$*"; }

UPSTREAM=${SYNORA_UPSTREAM:-"https://deb.freexian.com/extended-lts"}
BASE_PATH="${SYNORA_STORAGE}"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$apt_sync" --delete "${UPSTREAM}" @auto @auto @auto "$BASE_PATH"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
