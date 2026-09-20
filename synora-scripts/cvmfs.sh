#!/bin/bash
# requires: createrepo reposync wget curl
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
export SYNORA_REPOSITORY=cvmfs
apt_sync="${_here}/apt-sync.py" 
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL="${SYNORA_UPSTREAM:-"https://cvmrepo.s3-website.cern.ch/cvmrepo"}"

APT_PATH="${BASE_PATH}/apt"
YUM_PATH="${BASE_PATH}/yum"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$apt_sync" --delete "${BASE_URL}/apt" @auto @auto @auto "$APT_PATH"

# =================== YUM/DNF repos ==========================
"$yum_sync" "${BASE_URL}/yum/@{comp}/EL/@{os_ver}/@{arch}" @auto cvmfs,cvmfs-config,cvmfs-kernel @auto "@{comp}-EL@{os_ver}-@{arch}" "$YUM_PATH"
echo "YUM finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
