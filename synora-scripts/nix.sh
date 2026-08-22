#!/bin/bash

SYNORA_STORAGE="${SYNORA_STORAGE:-nix}"
SYNORA_UPSTREAM="${SYNORA_UPSTREAM:-s3://nix-releases/nix/}"
MIRROR_BASE_URL="${MIRROR_BASE_URL:-https://mirrors.tuna.tsinghua.edu.cn/nix}"
ORIG_BASE_URL_OLD="https://nixos.org/releases/nix"
ORIG_BASE_URL="https://releases.nixos.org/nix"

EXCLUDES=(--exclude "*/*/*" \
    --exclude "nix-[01].*" \
    --exclude "nix-2.[01][./]*" \
    --exclude "*-broken*")

INSTALL_TEMP="$(mktemp -d .tmp.XXXXXX)"
trap 'rm -rf "$INSTALL_TEMP"' EXIT

[[ ! -d "${SYNORA_STORAGE}" ]] && mkdir -p "${SYNORA_STORAGE}"
cd "${SYNORA_STORAGE}"
aws --no-sign-request s3 sync ${SYNORA_AWS_OPTIONS} \
    "${EXCLUDES[@]}" \
    --exclude "*/install" \
    --exclude "*/install.asc" \
    --exclude "*/install.sha256" \
    "${SYNORA_UPSTREAM}" .

# Create install script

aws --no-sign-request s3 sync ${SYNORA_AWS_OPTIONS} \
    --exclude "*" \
    --include "*/install" \
    "${EXCLUDES[@]}" \
    "${SYNORA_UPSTREAM}" "${INSTALL_TEMP}"

for version in $(ls "$INSTALL_TEMP"); do
    [[ ! -d "${version}" ]] && continue # Shouldn't happen

    sed -e "s|${ORIG_BASE_URL}|${MIRROR_BASE_URL}|" -e "s|${ORIG_BASE_URL_OLD}|${MIRROR_BASE_URL}|" \
        < "${INSTALL_TEMP}/${version}/install" \
        > "${INSTALL_TEMP}/${version}/.install"
    mv "${INSTALL_TEMP}/${version}/.install" "${version}/install"

    sha256sum "${version}/install" | cut -d' ' -f1 | tr -d '\n' \
        > "${INSTALL_TEMP}/${version}/.install.sha256"
    mv "${INSTALL_TEMP}/${version}/.install.sha256" "${version}/install.sha256"
done

ln -sfn "$(ls -d nix-* | sort -rV | head -1)" latest
