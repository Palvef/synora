#!/bin/bash
# requires: wget curl
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://storage.googleapis.com/bazel-apt"}

export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$apt_sync" --delete "$BASE_URL" stable jdk1.8 amd64 "$BASE_PATH"

echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
