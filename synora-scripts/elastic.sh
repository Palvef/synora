#!/bin/bash
set -e
set -o pipefail

_here=`dirname $(realpath $0)`
export SYNORA_REPOSITORY=elastic
apt_sync="${_here}/apt-sync.py" 
yum_sync="${_here}/yum-sync.py"

BASE_URL=${SYNORA_UPSTREAM:-"https://artifacts.elastic.co"}

BASE_PATH="${SYNORA_STORAGE%/}"
BASE_URL="${BASE_URL%/}"

versions=$(python3 "${_here}/helpers/repo_discovery.py" elastic)
mapfile -t ELASTIC_VERSION <<< "$versions"

YUM_PATH="${BASE_PATH}/yum"
APT_PATH="${BASE_PATH}/apt"
export REPO_SIZE_FILE=/tmp/reposize.$RANDOM

# =================== APT repos ===============================

for elsver in "${ELASTIC_VERSION[@]}"; do
	"$apt_sync" --delete "${BASE_URL}/packages/${elsver}/apt" stable main @auto "${APT_PATH}/${elsver}"
	mkdir -p "${BASE_PATH}/${elsver}"
	ln -sfnr "${APT_PATH}/${elsver}" "${BASE_PATH}/${elsver}/apt"
done

# # ================ YUM/DNF repos ===============================
components="${ELASTIC_VERSION[@]}"
components=${components// /,}
"$yum_sync" "${BASE_URL}/packages/@{comp}/yum" unused "$components" @auto "elastic-@{comp}" "$YUM_PATH"

for elsver in ${ELASTIC_VERSION[@]}; do
	mkdir -p "${BASE_PATH}/${elsver}"
	ln -sfnr "${YUM_PATH}/elastic-${elsver}" "${BASE_PATH}/${elsver}/yum"
done

"${_here}/helpers/size-sum.sh" $REPO_SIZE_FILE --rm
