#!/bin/bash
set -e

_here=`dirname $(realpath $0)`
apt_sync="${_here}/apt-sync.py"
yum_sync="${_here}/yum-sync.py"

BASE_PATH="${SYNORA_STORAGE}"
BASE_URL=${SYNORA_UPSTREAM:-"http://repo.mongodb.org"}

YUM_PATH="${BASE_PATH}/yum"
APT_PATH="${BASE_PATH}/apt"
UBUNTU_PATH="${APT_PATH}/ubuntu"
DEBIAN_PATH="${APT_PATH}/debian"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

"$yum_sync" "${BASE_URL}/yum/redhat/@{os_ver}/mongodb-org/@{comp}/@{arch}/" @auto @auto x86_64 "el@{os_ver}-@{comp}" "$YUM_PATH"
"$apt_sync" --delete "$BASE_URL/apt/ubuntu" @auto/mongodb-org/@auto @auto @auto "$UBUNTU_PATH"
"$apt_sync" --delete "$BASE_URL/apt/debian" @auto/mongodb-org/@auto @auto @auto "$DEBIAN_PATH"
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
