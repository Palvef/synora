#!/bin/bash
set -e

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"https://repo.mongodb.org"}

YUM_PATH="${BASE_PATH}/yum"
APT_PATH="${BASE_PATH}/apt"
UBUNTU_PATH="${APT_PATH}/ubuntu"
DEBIAN_PATH="${APT_PATH}/debian"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

MONGO_VERSIONS="${MONGO_VERSIONS:-8.0,7.0,6.0,5.0,4.4,4.2}"
ubuntu_suites=""
debian_suites=""
IFS=, read -ra versions <<< "$MONGO_VERSIONS"
for version in "${versions[@]}"; do
    ubuntu_suites+="@{ubuntu-lts}/mongodb-org/${version},"
    debian_suites+="@{debian-current}/mongodb-org/${version},"
done

status=0
"$yum_sync" --legacy-x86-repo-names "${BASE_URL}/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}/" @rhel-current "$MONGO_VERSIONS" x86_64 "el@{os_ver}-@{comp}-@{arch}" "$YUM_PATH" || status=1
"$apt_sync" --delete "$BASE_URL/apt/ubuntu" "${ubuntu_suites%,}" multiverse amd64,i386,arm64 "$UBUNTU_PATH" || status=1
"$apt_sync" --delete "$BASE_URL/apt/debian" "${debian_suites%,}" main amd64,i386 "$DEBIAN_PATH" || status=1
if [ "$status" -ne 0 ]; then exit "$status"; fi
# Stable aliases follow the newest actually synchronized version, per distribution.
python3 - "$BASE_PATH" <<'PYALIASES'
from pathlib import Path
import sys,re
root=Path(sys.argv[1])
for parent in root.glob('apt/*/dists/*/mongodb-org'):
    versions=[p for p in parent.iterdir() if p.is_dir() and not p.is_symlink() and re.fullmatch(r'\d+\.\d+',p.name)]
    if versions:
        latest=max(versions,key=lambda p:tuple(map(int,p.name.split('.'))))
        temp=parent/'.stable.new';temp.unlink(missing_ok=True);temp.symlink_to(latest.name);temp.replace(parent/'stable')
PYALIASES

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm

# vim: ts=4 sts=4 sw=4
