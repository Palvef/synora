#!/bin/bash
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py" 

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL="${SYNORA_UPSTREAM:-"https://deb.xanmod.org/"}"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

APT_PATH="${BASE_PATH}"

# =================== APT repos ===============================
# see: https://deb.xanmod.org/dists/releases/InRelease
"$apt_sync" --delete "${BASE_URL/}" @ubuntu-lts,@debian-current main,non-free amd64,i386 "${APT_PATH}"
echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
