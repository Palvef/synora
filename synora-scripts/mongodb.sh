#!/bin/bash
set -e

_here=`dirname $(realpath $0)`
export SYNORA_REPOSITORY=mongodb
apt_sync="${_here}/apt-sync.py"
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://repo.mongodb.org"}

YUM_PATH="${BASE_PATH}/yum"
APT_PATH="${BASE_PATH}/apt"
UBUNTU_PATH="${APT_PATH}/ubuntu"
DEBIAN_PATH="${APT_PATH}/debian"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

status=0
"$yum_sync" --legacy-x86-repo-names "${BASE_URL}/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}/" @auto @auto @auto "el@{os_ver}-@{comp}-@{arch}" "$YUM_PATH" || status=1
"$apt_sync" --delete "$BASE_URL/apt/ubuntu" @auto/mongodb-org/@auto @auto @auto "$UBUNTU_PATH" || status=1
"$apt_sync" --delete "$BASE_URL/apt/debian" @auto/mongodb-org/@auto @auto @auto "$DEBIAN_PATH" || status=1
if [ "$status" -ne 0 ]; then exit "$status"; fi
python3 "${_here}/helpers/mongodb_aliases.py" "$BASE_PATH"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm

# vim: ts=4 sts=4 sw=4
