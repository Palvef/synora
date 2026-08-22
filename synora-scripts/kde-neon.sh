#!/bin/bash
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"http://archive.neon.kde.org"}

APT_PATH="${BASE_PATH}/user"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

# =================== APT repos ===============================
"$apt_sync" --delete "${BASE_URL}/user" @ubuntu-lts main dep11,cnf,all,amd64,i386 "${BASE_PATH}/user"
echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
