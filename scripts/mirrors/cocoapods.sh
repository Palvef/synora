#!/bin/bash
set -e

function repo_init() {
	UPSTREAM=$1
	WORKING_DIR=$2
	git clone --mirror "$UPSTREAM" "$WORKING_DIR"
}

function update_cocoapods_git() {
	UPSTREAM="$1"
	repo_dir="$2"
	cd "$repo_dir"
	echo "==== SYNC $repo_dir START ===="
	git remote set-url origin "$UPSTREAM"
	set +e
	/usr/bin/timeout -s INT 3600 git remote -v update -p
	local ret=$?
	set -e
	if [[ $ret -ne 0 ]]; then
		echo "git update failed with rc=$ret"
		echo "==== SYNC $repo_dir FAILED ===="
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
	total_size=$((total_size + 1024 * sz))
	echo "==== SYNC $repo_dir DONE ===="
}

UPSTREAM_BASE=${TUNASYNC_UPSTREAM_URL:-"https://github.com/CocoaPods"}
REPOS=("Specs")
total_size=0

for repo in ${REPOS[@]}; do
	if [[ ! -d "$TUNASYNC_WORKING_DIR/${repo}.git" ]]; then
		echo "Initializing ${repo}.git"
		repo_init "${UPSTREAM_BASE}/${repo}.git" "$TUNASYNC_WORKING_DIR/${repo}.git"
	fi
	update_cocoapods_git "${UPSTREAM_BASE}/${repo}.git" "$TUNASYNC_WORKING_DIR/${repo}.git"
done

echo "Total size is" $(numfmt --to=iec $total_size)
echo "==== SYNC CocoaPods DONE ===="
