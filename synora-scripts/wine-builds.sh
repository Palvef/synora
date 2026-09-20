#!/bin/bash
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
export SYNORA_REPOSITORY=wine-builds
apt_sync="${_here}/apt-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://dl.winehq.org/wine-builds"}

export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$apt_sync" --delete "$BASE_URL/ubuntu" @ubuntu-lts main @auto "$BASE_PATH/ubuntu"
echo "APT for Ubuntu finished"

"$apt_sync" --delete "$BASE_URL/debian" @debian-current main @auto "$BASE_PATH/debian"
echo "APT for Debian finished"

echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
