#!/bin/bash
# requires: git, svn, wget
# pybombs-mirror: https://github.com/scateu/pybombs-mirror/

set -e
set -o pipefail

function pybombs_mirror() {
	[[ ! -d ${SYNORA_STORAGE} ]] && mkdir -p ${SYNORA_STORAGE}
	export PYBOMBS_MIRROR_BASE_URL=${MIRROR_BASE_URL}
	export PYBOMBS_MIRROR_WORK_DIR=${SYNORA_STORAGE}
	cp ${PYBOMBS_MIRROR_SCRIPT_PATH}/upstream-recipe-repos.urls ${SYNORA_STORAGE}/
	cp ${PYBOMBS_MIRROR_SCRIPT_PATH}/pre-replace-upstream.urls ${SYNORA_STORAGE}/
	cp ${PYBOMBS_MIRROR_SCRIPT_PATH}/ignore.urls ${SYNORA_STORAGE}/
	${PYBOMBS_MIRROR_SCRIPT_PATH}/pybombs-mirror.sh
}
function calculate_size() {
	total_size=0
	for repo in "${SYNORA_STORAGE}"/git/*; do
		sz=$(git -C "$repo" count-objects -v|grep -Po '(?<=size-pack: )\d+')
		total_size=$(($total_size+1024*$sz))
	done
	sz=$(du -sb "${SYNORA_STORAGE}/wget"|cut -f1)
	total_size=$(($total_size+$sz))
	echo "Total size is" $(numfmt --to=iec $total_size)
}

PYBOMBS_MIRROR_SCRIPT_PATH="${PYBOMBS_MIRROR_SCRIPT_PATH:-"/opt/pybombs-mirror"}"
MIRROR_BASE_URL="${MIRROR_BASE_URL:-"https://pybombs.tuna.tsinghua.edu.cn"}"

pybombs_mirror
calculate_size
