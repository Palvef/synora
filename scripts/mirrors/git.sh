#!/bin/bash
set -e

UPSTREAM=${TUNASYNC_UPSTREAM_URL}
if [[ -z "$UPSTREAM" ]]; then
	echo "Please set the TUNASYNC_UPSTREAM_URL"
	echo "==== SYNC git FAILED ===="
	exit 1
fi

function repo_init() {
	git clone --mirror "$UPSTREAM" "$TUNASYNC_WORKING_DIR"
}

function update_linux_git() {
	cd "$TUNASYNC_WORKING_DIR"
	echo "==== SYNC $UPSTREAM START ===="
	git remote set-url origin "$UPSTREAM"
	set +e
	/usr/bin/timeout -s INT 3600 git remote -v update -p
	local ret=$?
	set -e
	if [[ $ret -ne 0 ]]; then
		echo "git update failed with rc=$ret"
		echo "==== SYNC $UPSTREAM FAILED ===="
		return $ret
	fi
	local head
	head=$(git remote show origin | awk '/HEAD branch:/ {print $NF}')
	[[ -n "$head" ]] && echo "ref: refs/heads/$head" > HEAD
	echo "counting loose objects..."
	local loose
	loose=$(find objects -type f ! -path 'objects/pack/*' | wc -l)
	echo "loose objects: $loose"
	if [[ "$loose" -gt 50 ]]; then
		echo "repacking loose objects..."
		git repack -a -b -d
	fi
	local sz
	sz=$(git count-objects -v | grep -Po '(?<=size-pack: )\d+')
	sz=$((sz * 1024))
	echo "Total size is" $(numfmt --to=iec $sz)
	echo "==== SYNC $UPSTREAM DONE ===="
	return 0
}

if [[ ! -f "$TUNASYNC_WORKING_DIR/HEAD" ]]; then
	echo "Initializing $UPSTREAM mirror"
	repo_init
fi

update_linux_git
