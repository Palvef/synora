#!/bin/bash
# requires: lftp wget jq
set -e
set -o pipefail

BASE_URL="${SYNORA_UPSTREAM:-"http://images.linuxcontainers.org"}"

function sync_lxc_images() {
	repo_url="$1"
	repo_dir="$2"

	[[ ! -d "$repo_dir" ]] && mkdir -p "$repo_dir"
	cd "$repo_dir"

	lftp "${repo_url}" -e "mirror --verbose -P 5 ; bye"
	echo "lftp returns $?"
}


echo "=== Downloading /meta/1.0 ==="
mkdir -p "${SYNORA_STORAGE}/meta/1.0"
for i in index-system index-system.asc index-user index-user.asc; do
  wget -O "${SYNORA_STORAGE}/meta/1.0/$i.work-in-progress" "${BASE_URL}/meta/1.0/$i"
done

echo "=== Downloading /streams/v1 ==="
mkdir -p "${SYNORA_STORAGE}/streams/v1"
wget -O "${SYNORA_STORAGE}/streams/v1/index.json.work-in-progress" "${BASE_URL}/streams/v1/index.json"

jq -r '.index[].path' "${SYNORA_STORAGE}/streams/v1/index.json.work-in-progress" | while read line; do
    [[ ! -d "${SYNORA_STORAGE}/$(dirname $line)" ]] && mkdir -p "${SYNORA_STORAGE}/$(dirname $line)"
    wget -O "${SYNORA_STORAGE}/${line}.work-in-progress" "${BASE_URL}/${line}"
done

echo "=== Downloading images ==="

sync_lxc_images "${BASE_URL}/images" "${SYNORA_STORAGE}/images"

images_json="${SYNORA_STORAGE}/streams/v1/images.json.work-in-progress"
[[ -f "$images_json" ]] || exit 1
jq -r '.products[].versions[].items[].path' "$images_json" > /tmp/filelist.txt

cat /tmp/filelist.txt | while read line; do
    # $line looks like 'images/ubuntu/xenial/armhf/default/20200219_07:42/rootfs.tar.xz'
    if [[ ! -f "${SYNORA_STORAGE}/${line}" ]]; then
        echo "Error: ${SYNORA_STORAGE}/${line} vanished"
        exit 1
    fi
done

echo "=== Replacing /meta/1.0 ==="
for i in index-system index-system.asc index-user index-user.asc; do
  mv -f "${SYNORA_STORAGE}/meta/1.0/$i.work-in-progress" "${SYNORA_STORAGE}/meta/1.0/$i"
done

echo "=== Replacing /streams/v1 ==="
jq -r '.index[].path' "${SYNORA_STORAGE}/streams/v1/index.json.work-in-progress" | while read line; do
    mv -f "${SYNORA_STORAGE}/${line}.work-in-progress" "${SYNORA_STORAGE}/${line}"
done
mv -f "${SYNORA_STORAGE}/streams/v1/index.json.work-in-progress" "${SYNORA_STORAGE}/streams/v1/index.json"

echo "=== Removing old images ==="

cd "${SYNORA_STORAGE}"

find images/ -maxdepth 5 -mindepth 5 -mtime +3 | while read line; do
    # $line looks like 'images/ubuntu/xenial/armhf/default/20200217_07:42'
    grep --quiet "$line" /tmp/filelist.txt || ( echo "Removing $line"; rm -rf "$line" )
done
