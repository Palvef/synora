#!/bin/bash
# reqires: wget, yum-utils
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
export SYNORA_REPOSITORY=gitlab-runner
apt_sync="${_here}/apt-sync.py"
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
UPSTREAM=${SYNORA_UPSTREAM:-"https://packages.gitlab.com/runner/gitlab-runner"}

YUM_PATH="${BASE_PATH}/yum"
UBUNTU_PATH="${BASE_PATH}/ubuntu/"
DEBIAN_PATH="${BASE_PATH}/debian/"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$yum_sync" "${UPSTREAM}/el/@{os_ver}/@{arch}" @rhel-current gitlab-runner @auto "el@{os_ver}-@{arch}" "$YUM_PATH"
echo "YUM finished"

"$apt_sync" --delete "${UPSTREAM}/ubuntu" @ubuntu-lts main @auto "$UBUNTU_PATH"
echo "Ubuntu finished"
"$apt_sync" --delete "${UPSTREAM}/debian" @debian-current main @auto "$DEBIAN_PATH"
echo "Debian finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm

# vim: ts=4 sts=4 sw=4
