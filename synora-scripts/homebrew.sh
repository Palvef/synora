#!/bin/bash
set -e

. /usr/lib/synora/scripts/helpers/git_mirror.sh

UPSTREAM_BASE=${SYNORA_UPSTREAM:-"https://github.com/Homebrew"}
brews=("brew" "homebrew-core" "homebrew-cask" "install" "homebrew-command-not-found" "homebrew-services")
total_size=0

for brew in ${brews[@]}; do
	repo="$SYNORA_STORAGE/${brew}.git"
	if [[ ! -d "$repo" ]]; then
		echo "Initializing ${brew}.git"
		git_mirror_init "${UPSTREAM_BASE}/${brew}.git" "$repo"
	fi
	git_mirror_update "${UPSTREAM_BASE}/${brew}.git" "$repo"
	total_size=$((total_size + GIT_MIRROR_BYTES))
done

echo "Total size is" "$(numfmt --to=iec $total_size)"
echo "SYNORA_SIZE=$total_size"
