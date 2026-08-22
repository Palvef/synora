#!/bin/bash
set -e

git_cmd=(git -c user.email=mirrors@hernet -c user.name="hernet mirrors")

function repo_init() {
        UPSTREAM="$1"
        WORKING_DIR="$2"
        git clone --mirror "$UPSTREAM" "$WORKING_DIR" 
}

function update_font_git() {
	UPSTREAM="$1"
        repo_dir="$2"
        cd "$repo_dir"
        echo "==== SYNC $repo_dir START ===="
	git remote set-url origin "$UPSTREAM"
        /usr/bin/timeout -s INT 3600 git remote update --prune
	head=$(git remote show origin | awk '/HEAD branch:/ {print $NF}')
	[[ -n "$head" ]] && echo "ref: refs/heads/$head" > HEAD
        loose=$(git count-objects -v | awk '/^count:/{print $2}')
        [[ "${loose:-0}" -gt 50 ]] && git repack -a -b -d
        sz=$(git count-objects -v|grep -Po '(?<=size-pack: )\d+')
        total_size=$(($total_size+1024*$sz))
        echo "==== SYNC $repo_dir DONE ===="
}

function checkout_font_branch() {
	repo_dir="$1"
	work_tree="$2"
	branch="$3"
	echo "Checkout branch $branch to $work_tree"
	if [[ ! -d "$2" ]]; then
		"${git_cmd[@]}" clone "$repo_dir" --branch "$branch" --single-branch "$work_tree"
	else
		cd "$work_tree"
		"${git_cmd[@]}" pull
	fi
}

UPSTREAM_BASE=${SYNORA_UPSTREAM:-"https://github.com/adobe-fonts"}
REPOS=("source-code-pro" "source-sans-pro" "source-serif-pro" "source-han-sans" "source-han-serif")
total_size=0

for repo in ${REPOS[@]}; do
        if [[ ! -d "$SYNORA_STORAGE/${repo}.git" ]]; then
                echo "Initializing ${repo}.git"
                repo_init "${UPSTREAM_BASE}/${repo}.git" "$SYNORA_STORAGE/${repo}.git"
        fi
        update_font_git "${UPSTREAM_BASE}/${repo}.git" "$SYNORA_STORAGE/${repo}.git"
	checkout_font_branch "$SYNORA_STORAGE/${repo}.git" "$SYNORA_STORAGE/${repo}" "release"
done

echo "Total size is" $(numfmt --to=iec $total_size)
