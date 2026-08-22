#!/bin/bash
# requires: wget, rsync
#

set -e
set -o pipefail

UPSTREAM=${SYNORA_UPSTREAM:-"rsync://rpms.remirepo.net"}
REPOS=("enterprise" "fedora")

RSYNC_OPTS="-aHvh --no-o --no-g --stats --exclude .~tmp~/ --delete --delete-excluded --delete-after --delay-updates --safe-links --timeout=120 --contimeout=120"

USE_IPV6=${USE_IPV6:-"0"}
if [[ $USE_IPV6 == "1" ]]; then
	RSYNC_OPTS="-6 ${RSYNC_OPTS}"
fi


for repo in ${REPOS[@]}; do
	upstream=${UPSTREAM}/${repo}
	dest=${SYNORA_STORAGE}/${repo}

	[ ! -d "$dest" ] && mkdir -p "$dest"
	
	rsync ${RSYNC_OPTS} "$upstream" "$dest"
done

wget -O ${SYNORA_STORAGE}/index.html http://rpms.remirepo.net/index.html
wget -O ${SYNORA_STORAGE}/PRM-GPG-KEY-remi http://rpms.remirepo.net/RPM-GPG-KEY-remi
