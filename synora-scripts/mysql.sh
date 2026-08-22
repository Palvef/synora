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

# MySQL APT is not a uniform distro/arch matrix.
# jammy/noble still ship mysql-8.0; resolute dropped it.
# Debian bullseye/bookworm still ship mysql-8.0 and i386.
# Debian trixie is amd64-only and has no mysql-8.0.
# Colon lists in apt-sync.py are per-codename, in os_version order.
"$apt_sync" --delete \
    "${BASE_URL}/apt/ubuntu" \
    jammy,noble,resolute \
    mysql-tools,mysql-8.0,mysql-8.4-lts:mysql-tools,mysql-8.0,mysql-8.4-lts:mysql-tools,mysql-8.4-lts \
    amd64,i386 \
    "${UBUNTU_PATH}"
echo "Ubuntu finished"

"$apt_sync" --delete \
    "${BASE_URL}/apt/debian" \
    bullseye,bookworm,trixie \
    mysql-tools,mysql-8.0,mysql-8.4-lts:mysql-tools,mysql-8.0,mysql-8.4-lts:mysql-tools,mysql-8.4-lts \
    amd64,i386:amd64,i386:amd64 \
    "${DEBIAN_PATH}"
echo "Debian finished"

# =================== YUM/DNF repos ==========================
COMPONENTS="mysql-connectors-community,mysql-tools-community,mysql-8.0-community,mysql-8.4-community"
"$yum_sync" "${BASE_URL}/yum/@{comp}/el/@{os_ver}/@{arch}/" @rhel-current "$COMPONENTS" x86_64,aarch64 "@{comp}-el@{os_ver}-@{arch}" "$YUM_PATH"
echo "YUM finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
