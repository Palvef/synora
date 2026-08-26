#!/bin/bash
set -e

REPO=${REPO:-"/usr/local/bin/repo"}
USE_BITMAP_INDEX=${USE_BITMAP_INDEX:-"0"}
UPSTREAM=${SYNORA_UPSTREAM:-"https://android.googlesource.com/mirror/manifest"}
AOSP_SYNC_JOBS=${AOSP_SYNC_JOBS:-"4"}

if [[ ! "$AOSP_SYNC_JOBS" =~ ^[1-9][0-9]*$ ]]; then
	echo "AOSP_SYNC_JOBS must be a positive integer" >&2
	exit 2
fi

git config --global user.email "mirrors@hernet"
git config --global user.name "hernet mirrors"

repo_sync_rc=0

function repo_init() {
	mkdir -p $SYNORA_STORAGE
	cd $SYNORA_STORAGE
	$REPO init -u $UPSTREAM --mirror
}

function repo_sync() {
	cd $SYNORA_STORAGE
	set +e
	$REPO sync -f -j"$AOSP_SYNC_JOBS"
	repo_sync_rc=$?
	set -e
	if [[ "$repo_sync_rc" -ne 0 ]]; then
		echo "WARNING: repo-sync may fail, but we just ignore it."
	fi
}

function git_repack() {
	echo "Start writing bitmap index"
	while read repo; do 
		cd $repo
		size=$(du -sk .|cut -f1)
		total_size=$(($total_size+1024*$size))
		loose=$(git count-objects -v | awk '/^count:/{print $2}')
		if [[ "${loose:-0}" -gt 50 && "$size" -gt "100000" ]]; then
			git repack -a -b -d
		fi
	done < <(find $SYNORA_STORAGE -type d -not -path "*/.repo/*" -name "*.git")
}

if [[ ! -d "$SYNORA_STORAGE/git-repo.git" ]]; then
	echo "Initializing AOSP mirror"
	repo_init
fi

repo_sync

total_size=0
if [[ "$USE_BITMAP_INDEX" == "1" ]]; then
	git_repack
	echo "Total size is" $(numfmt --to=iec $total_size)
fi
exit $repo_sync_rc
