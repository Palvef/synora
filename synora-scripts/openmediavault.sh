#!/bin/bash
# requires: wget curl
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
export SYNORA_REPOSITORY=openmediavault
apt_sync="${_here}/apt-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://packages.openmediavault.org/public"}
DISTS=@auto
EXTRA_DISTS=@auto
ARCHS=@auto
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

# =================== official repos ===============================
"$apt_sync" --delete "${BASE_URL}" "$DISTS" main,partner $ARCHS "${BASE_PATH}/public"
"$apt_sync" --delete "https://openmediavault.github.io/packages" "$DISTS" main,partner $ARCHS "${BASE_PATH}/packages"
# =================== extra repos ===============================
wget -O "${BASE_PATH}/openmediavault-plugin-developers/omvextras2026.asc" https://openmediavault-plugin-developers.github.io/packages/debian/omvextras2026.asc
"$apt_sync" --delete "https://openmediavault-plugin-developers.github.io/packages/debian" \
    "$EXTRA_DISTS" main $ARCHS "${BASE_PATH}/openmediavault-plugin-developers"
echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
