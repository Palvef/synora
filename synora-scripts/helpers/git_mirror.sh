# Shared git-mirror helpers (tunasync git.sh behavior, without verbose
# ref listing or `find objects` walks that pin CPU and never look "done").
# shellcheck shell=bash

git_mirror_clean_pack_tmp() {
	local repo_dir="$1"
	local pack="$repo_dir/objects/pack"
	if [[ -d "$repo_dir/.git/objects/pack" ]]; then
		pack="$repo_dir/.git/objects/pack"
	fi
	if [[ ! -d "$pack" ]]; then
		return 0
	fi
	find "$pack" -maxdepth 1 -type f \( \
		-name 'tmp_pack_*' -o \
		-name 'tmp_idx_*' -o \
		-name 'tmp_obj_*' -o \
		-name 'tmp_bitmap_*' -o \
		-name '.tmp-*' \
	\) -delete 2>/dev/null || true
}

git_mirror_init() {
	local upstream="$1"
	local repo_dir="$2"
	git clone --mirror "$upstream" "$repo_dir"
}

git_mirror_update() {
	local upstream="$1"
	local repo_dir="$2"
	cd "$repo_dir" || return 1
	git_mirror_clean_pack_tmp "$repo_dir"
	echo "==== SYNC $repo_dir START ===="
	git remote set-url origin "$upstream"
	local ret=0
	set +e
	/usr/bin/timeout -s INT 3600 git remote update --prune
	ret=$?
	set -e
	if [[ "$ret" -ne 0 ]]; then
		echo "git update failed with rc=$ret"
		echo "==== SYNC $repo_dir FAILED ===="
		return "$ret"
	fi
	local head
	head=$(git remote show origin | awk '/HEAD branch:/ {print $NF}')
	if [[ -n "$head" && "$head" != "(unknown)" ]]; then
		echo "ref: refs/heads/$head" >HEAD
	fi
	local loose
	loose=$(git count-objects -v | awk '/^count:/{print $2}')
	loose=${loose:-0}
	echo "loose objects: $loose"
	if [[ "$loose" -gt 50 ]]; then
		echo "repacking loose objects..."
		git repack -a -b -d
	fi
	local packed extra
	packed=$(git count-objects -v | awk '/^size-pack:/{print $2}')
	extra=$(git count-objects -v | awk '/^size:/{print $2}')
	packed=${packed:-0}
	extra=${extra:-0}
	GIT_MIRROR_BYTES=$(( (packed + extra) * 1024 ))
	echo "==== SYNC $repo_dir DONE ===="
	return 0
}

git_loose_count() {
	git count-objects -v | awk '/^count:/{print $2}'
}
