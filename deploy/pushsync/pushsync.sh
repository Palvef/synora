#!/bin/bash
# Forced push trigger for Synora jobs. Upstream mirrors SSH in as `tunasync`
# on port 22222; KEY_ID from authorized_keys selects allowed job names.
set -euo pipefail

KEY_ID="${KEY_ID:?No KEY_ID provided in environment}"
KEY_REPO_MAP="${KEY_REPO_MAP:-/home/tunasync/key_repo_map.conf}"
ARCHIVE_PREFIX="sync:archive:"
ENV_FILE="${SYNORA_ENV_FILE:-/home/tunasync/synora.env}"

if [[ -f "$ENV_FILE" ]]; then
	# shellcheck disable=SC1090
	. "$ENV_FILE"
fi
SYNORA_API="${SYNORA_API:?SYNORA_API is required (set in $ENV_FILE)}"
SYNORA_TOKEN="${SYNORA_TOKEN:?SYNORA_TOKEN is required (set in $ENV_FILE)}"

FILTERED_ARGS=()
skip_next=0
for arg in "$@"; do
	if [[ "$skip_next" = 1 ]]; then
		IFS=' ' read -r -a FILTERED_ARGS <<< "$arg"
		break
	fi
	if [[ "$arg" = "-c" ]]; then
		skip_next=1
		continue
	fi
	FILTERED_ARGS+=("$arg")
done

if [[ "${#FILTERED_ARGS[@]}" -eq 0 ]]; then
	echo "Usage: ssh tunasync@host [repo] [sync:archive:repo] ..." >&2
	exit 2
fi

declare -A ALLOWED
while read -r key repos _worker; do
	[[ -z "$key" || "$key" =~ ^# ]] && continue
	[[ "$key" != "$KEY_ID" ]] && continue
	IFS=',' read -ra repo_list <<< "$repos"
	for repo in "${repo_list[@]}"; do
		ALLOWED["$repo"]=1
	done
done < "$KEY_REPO_MAP"

if [[ "${#ALLOWED[@]}" -eq 0 ]]; then
	echo "No allowed repos for key ID: $KEY_ID" >&2
	exit 3
fi

auth_header="Authorization: Bearer ${SYNORA_TOKEN}"
api="${SYNORA_API%/}"

trigger() {
	local repo="$1"
	# Force: cancel a running instance, then queue a new run.
	curl -fsS -X POST -H "$auth_header" "${api}/api/v1/jobs/${repo}/stop" >/dev/null || true
	local run_id
	run_id=$(curl -fsS -X POST -H "$auth_header" "${api}/api/v1/jobs/${repo}/run")
	echo "$repo: start syncing... ($run_id)"
}

for raw_repo in "${FILTERED_ARGS[@]}"; do
	repo="$raw_repo"
	if [[ "$repo" == "$ARCHIVE_PREFIX"* ]]; then
		repo="${repo#$ARCHIVE_PREFIX}"
	fi
	if [[ -n "${ALLOWED[$repo]:-}" ]]; then
		trigger "$repo"
	else
		echo "Unauthorized repo: $repo (for key $KEY_ID)" >&2
		exit 4
	fi
done
