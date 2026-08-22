#!/bin/bash
set -e

git_cmd=(git -c user.email=mirrors@hernet -c user.name="hernet mirrors")

function repo_init() {
        UPSTREAM="$1"
        WORKING_DIR="$2"
        git clone "$UPSTREAM" "$WORKING_DIR"
}

function update_git() {
        UPSTREAM="$1"
        repo_dir="$2"
        cd "$repo_dir"
        echo "==== SYNC $repo_dir START ===="
        git remote set-url origin "$UPSTREAM"
        timeout -s INT 3600 git remote update --prune
        head=$(git remote show origin | awk '/HEAD branch:/ {print $NF}')
        [[ -n "$head" ]] && echo "ref: refs/heads/$head" > HEAD
        loose=$(git count-objects -v | awk '/^count:/{print $2}')
        [[ "${loose:-0}" -gt 50 ]] && git repack -a -b -d
        sz=$(git count-objects -v|grep -Po '(?<=size-pack: )\d+')
        total_size=$(($total_size+1024*$sz))
        echo "==== SYNC $repo_dir DONE ===="
}

function checkout_branch() {
        repo_dir="$1"
        work_tree="$2"
        branch="$3"
        echo "Checkout branch $branch to $work_tree"
        if [[ ! -d "$2" ]]; then
                "${git_cmd[@]}" clone "$repo_dir" --branch "$branch" --single-branch "$work_tree"
        else
                cd "$work_tree"
                "${git_cmd[@]}" checkout -B "$branch" "origin/$branch"
        fi
}

UPSTREAM_BASE=${SYNORA_UPSTREAM}
UPSTREAM_BRANCH=${UPSTREAM_BRANCH:-"master"}
total_size=0

if [[ ! -d "$SYNORA_STORAGE/.git" ]]; then
        echo "Initializing"
        repo_init "${UPSTREAM_BASE}" "$SYNORA_STORAGE"
fi
update_git "${UPSTREAM_BASE}" "$SYNORA_STORAGE/.git"
checkout_branch "$SYNORA_STORAGE/.git" "$SYNORA_STORAGE" "$UPSTREAM_BRANCH"

echo "Total size is" $(numfmt --to=iec $total_size)
