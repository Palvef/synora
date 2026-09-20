#!/bin/bash
# requires: wget, timeout
set -e
set -o pipefail

_here=$(dirname "$(realpath "$0")")
apt_sync="${_here}/apt-sync.py" 

# Older Synora managers expose a CONNECT-only proxy. Wrap the complete job so
# plain HTTP used by Proxmox (HTTPS is not valid for this hostname) is tunneled
# instead of receiving 405 from the parent proxy.
connect_wrapper="${_here}/helpers/http_connect_proxy.py"
assigned_proxy=${HTTP_PROXY:-${http_proxy:-${ALL_PROXY:-${all_proxy:-}}}}
if [ "${SYNORA_HTTP_CONNECT_WRAPPED:-}" != "1" ] && [ -f "$connect_wrapper" ]; then
	case "$assigned_proxy" in
		http://*|https://*)
			export SYNORA_HTTP_CONNECT_WRAPPED=1
			exec python3 "$connect_wrapper" -- "$0" "$@"
			;;
	esac
fi

source "${_here}/helpers/tuna-scope.sh"
prepare_tuna_scope proxmox
# With a TUNA policy, discover upstream suites and intersect the pinned scope.
suites=@debian-current
if [ "${SYNC_SCOPE_POLICY:-}" = tuna ]; then suites=@auto; fi

BASE_URL="${SYNORA_UPSTREAM:-"http://download.proxmox.com"}"
BASE_PATH="${SYNORA_STORAGE}"

APT_PATH="${BASE_PATH}/debian"
PVE_PATH="${APT_PATH}/pve"
PBS_PATH="${APT_PATH}/pbs"
PBS_CLIENT_PATH="${APT_PATH}/pbs-client"
PMG_PATH="${APT_PATH}/pmg"

# === download deb packages ====

"$apt_sync" --delete "${BASE_URL}/debian/pve" "$suites" pve-no-subscription,pvetest amd64 "$PVE_PATH"
"$apt_sync" --delete "${BASE_URL}/debian/pbs" "$suites" pbs-no-subscription amd64 "$PBS_PATH"
"$apt_sync" --delete "${BASE_URL}/debian/pbs-client" "$suites" main amd64 "$PBS_CLIENT_PATH"
"$apt_sync" --delete "${BASE_URL}/debian/pmg" "$suites" pmg-no-subscription amd64 "$PMG_PATH"
# upstream directory structure
ln -sfnT pve/dists "$APT_PATH/dists"
echo "Debian finished"

# === download standalone files ====

function sync_files() {
	repo_url="$1"
	repo_dir="$2"

	[ ! -d "$repo_dir" ] && mkdir -p "$repo_dir"
	cd $repo_dir
	lftp "${repo_url}/" -e "mirror --verbose -P 5 --delete --only-newer; bye"
}

sync_files "${BASE_URL}/images" "${BASE_PATH}/images"
sync_files "${BASE_URL}/iso" "${BASE_PATH}/iso"

echo "Proxmox finished"
