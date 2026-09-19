#!/bin/bash
# requires: createrepo reposync wget curl
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py" 
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://repos.influxdata.com"}

YUM_PATH="${BASE_PATH}/yum"
UBUNTU_PATH="${BASE_PATH}/ubuntu"
DEBIAN_PATH="${BASE_PATH}/debian"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

# =================== APT repos ===============================

# Discover every published suite and its declared components/architectures.
# This also preserves historical aliases without inventing missing distro codenames.
"$apt_sync" --delete "${BASE_URL}/debian" @auto @auto @auto "$DEBIAN_PATH"
ln -sTf debian "$UBUNTU_PATH"
echo "Debian/Ubuntu finished"


# =================== YUM/DNF repos ==========================
"$yum_sync" "${BASE_URL}/stable/@{arch}/main/" stable influxdata @auto "stable-@{arch}" "$YUM_PATH"
echo "YUM finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
