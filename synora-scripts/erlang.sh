#!/bin/bash
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py" 
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://packages.erlang-solutions.com"}

CENTOS_PATH="${BASE_PATH}/centos"
UBUNTU_PATH="${BASE_PATH}/ubuntu"
DEBIAN_PATH="${BASE_PATH}/debian"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$apt_sync" --delete "${BASE_URL}/ubuntu" @auto @auto @auto "$UBUNTU_PATH"
"$apt_sync" --delete "${BASE_URL}/debian" @auto @auto @auto "$DEBIAN_PATH"

# =================== YUM repos ===============================
## DISABLED due to invalid repo structure

# "$yum_sync" "${BASE_URL}/centos/@{os_ver}/@{arch}" 7 erlang x86_64 "@{os_ver}" "$CENTOS_PATH"
# echo "CentOS finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
