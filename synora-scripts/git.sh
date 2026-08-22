#!/bin/bash
set -e

# tunasync git.sh: bare mirror clone + remote update --prune, HEAD, optional
# repack, size. Runs as the container command (docker --init --entrypoint).

. /usr/lib/synora/scripts/helpers/git_mirror.sh

UPSTREAM=${SYNORA_UPSTREAM}
if [[ -z "$UPSTREAM" ]]; then
	echo "Please set the SYNORA_UPSTREAM"
	echo "==== SYNC git FAILED ===="
	exit 1
fi

if [[ ! -f "$SYNORA_STORAGE/HEAD" ]]; then
	echo "Initializing $UPSTREAM mirror"
	git_mirror_init "$UPSTREAM" "$SYNORA_STORAGE"
fi

git_mirror_update "$UPSTREAM" "$SYNORA_STORAGE"
echo "Total size is" "$(numfmt --to=iec "$GIT_MIRROR_BYTES")"
echo "SYNORA_SIZE=$GIT_MIRROR_BYTES"
