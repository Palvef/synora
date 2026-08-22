#!/bin/bash
# requires: wget curl
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"http://packages.ros.org/ros2"}

APT_PATH="${BASE_PATH}/ubuntu"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

# =================== APT repos ===============================
"$apt_sync" --delete "${BASE_URL}/ubuntu" @ubuntu-lts,@debian-current main amd64,armhf,arm64 "$APT_PATH"
echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
