#!/bin/bash
# requires: git, svn, wget
# pybombs-mirror: https://github.com/scateu/pybombs-mirror/

set -e
set -o pipefail

function pybombs_mirror() {
	mkdir -p "${SYNORA_STORAGE:?SYNORA_STORAGE is required}"
	export PYBOMBS_MIRROR_BASE_URL=${MIRROR_BASE_URL}
	export PYBOMBS_MIRROR_WORK_DIR=${SYNORA_STORAGE}
	cp "${PYBOMBS_MIRROR_SCRIPT_PATH}/upstream-recipe-repos.urls" "${SYNORA_STORAGE}/"
	cp "${PYBOMBS_MIRROR_SCRIPT_PATH}/pre-replace-upstream.urls" "${SYNORA_STORAGE}/"
	cp "${PYBOMBS_MIRROR_SCRIPT_PATH}/ignore.urls" "${SYNORA_STORAGE}/"
    local repo
    shopt -s nullglob
    for repo in "${SYNORA_STORAGE}"/git/*; do
        [ -d "$repo" ] || continue
        git -C "$repo" config remote.origin.prune true
    done
	"${PYBOMBS_MIRROR_SCRIPT_PATH}/pybombs-mirror.sh"
}
function calculate_size() {
    local total_size
    total_size=$(du -sb "${SYNORA_STORAGE}" | cut -f1)
    echo "SYNORA_SIZE=$total_size"
}
function publish_repositories() {
    local repo count=0
    shopt -s nullglob
    # Publish dumb HTTP indexes so static nginx can serve cloned repositories.
    for repo in "${SYNORA_STORAGE}"/git/* "${SYNORA_STORAGE}"/recipes/*.git; do
        [ -d "$repo" ] || continue
        git -C "$repo" update-server-info
    done
    for repo in "${SYNORA_STORAGE}"/recipes/*.git; do
        git -C "$repo" rev-parse --verify HEAD >/dev/null
        count=$((count + 1))
    done
    if [ "$count" -eq 0 ]; then
        echo "No usable PyBOMBS recipes were produced" >&2
        exit 1
    fi
    if [ -s "${SYNORA_STORAGE}/failed.log" ]; then
        echo "Warning: $(wc -l < "${SYNORA_STORAGE}/failed.log") source downloads failed; paths follow:"
        cat "${SYNORA_STORAGE}/failed.log"
        echo "SYNORA_STATUS=success_with_warnings"
    fi
}

PYBOMBS_MIRROR_SCRIPT_PATH="${PYBOMBS_MIRROR_SCRIPT_PATH:-"/opt/pybombs-mirror"}"
MIRROR_BASE_URL="${MIRROR_BASE_URL:-"https://pybombs.tuna.tsinghua.edu.cn"}"

pybombs_mirror
publish_repositories
python3 "$(dirname "${BASH_SOURCE[0]}")/helpers/pybombs_cleanup.py" "${SYNORA_STORAGE}"
calculate_size
