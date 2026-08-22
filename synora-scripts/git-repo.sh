#!/bin/bash
UPSTREAM=${SYNORA_UPSTREAM:-"https://gerrit.googlesource.com/git-repo"}

function repo_init() {
	git clone --mirror $UPSTREAM $SYNORA_STORAGE
}

function update_repo_git() {
	cd $SYNORA_STORAGE
	echo "==== SYNC repo.git START ===="
	git remote set-url origin "$UPSTREAM"
	/usr/bin/timeout -s INT 3600 git remote -v update -p
	head=$(git remote show origin | awk '/HEAD branch:/ {print $NF}')
	[[ -n "$head" ]] && echo "ref: refs/heads/$head" > HEAD
	git repack -a -b -d
	sz=$(git count-objects -v|grep -Po '(?<=size-pack: )\d+')
	sz=$(($sz*1024))
	echo "Total size is" $(numfmt --to=iec $sz)
	echo "==== SYNC repo.git DONE ===="
}

function checkout_repo() {
    git -C $SYNORA_STORAGE show HEAD:repo > $SYNORA_STORAGE/git-repo
}

if [[ ! -f "$SYNORA_STORAGE/HEAD" ]]; then
	echo "Initializing repo.git mirror"
	repo_init
fi

update_repo_git
checkout_repo
