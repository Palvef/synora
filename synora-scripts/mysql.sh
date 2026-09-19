#!/bin/bash
# requires: createrepo reposync wget curl rsync
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL="${SYNORA_UPSTREAM:-"https://repo.mysql.com"}"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

YUM_PATH="${BASE_PATH}/yum"
APT_PATH="${BASE_PATH}/apt"
UBUNTU_PATH="${APT_PATH}/ubuntu"
DEBIAN_PATH="${APT_PATH}/debian"

# The upstream Release file supplies components and architectures for each suite.
"$apt_sync" --delete "${BASE_URL}/apt/ubuntu" @auto @auto @auto "$UBUNTU_PATH"
"$apt_sync" --delete "${BASE_URL}/apt/debian" @auto @auto @auto "$DEBIAN_PATH"

# =================== YUM/DNF repos ==========================
COMPONENTS="@auto"
"$yum_sync" "${BASE_URL}/yum/@{comp}/el/@{os_ver}/@{arch}/" @rhel-current "$COMPONENTS" x86_64,aarch64 "@{comp}-el@{os_ver}-@{arch}" "$YUM_PATH"
echo "YUM finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
