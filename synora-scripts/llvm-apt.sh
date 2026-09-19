#!/bin/bash
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://apt.llvm.org"}

export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

function get_codenames() {
    local os=$1
    local dist_meta_url="${BASE_URL}/${os}/conf/distributions"
    local codenames=$(curl -sSfL $dist_meta_url 2>/dev/null | grep -oP '^Codename: \K.*' | tr '\n' ',' | sed 's/,$//')
    if [ -z "$codenames" ]; then
        echo "Unable to fetch codenames from $dist_meta_url" >&2
        return 1
    fi
    echo "Codenames for $os: $codenames" >&2
    echo $codenames
}

oses=$(python3 "${_here}/helpers/repo_discovery.py" distros ubuntu-lts && python3 "${_here}/helpers/repo_discovery.py" distros debian-current)
for os in $oses; do
    codenames=$(get_codenames $os)
    "$apt_sync" --delete "$BASE_URL/$os" "$codenames" main amd64,arm64 "$BASE_PATH/$os"
done

echo "APT finished"

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
