#!/bin/bash
# ftpsync wrapper. Generates an archvsync config from SYNORA_* so debian/kali
# can run in synora-scripts without a bind-mounted ftpsync conf.
set -e
set -o pipefail
set -u

export LOGNAME="${LOGNAME:-synora}"

FTPSYNC="${FTPSYNC:-ftpsync}"
FTPSYNC_LOG_DIR="${FTPSYNC_LOG_DIR:-/var/log/ftpsync}"
FTPSYNC_ETC="${FTPSYNC_ETC:-/ftpsync/etc}"
SLEEP="${FTPSYNC_SLEEP:-120}"

trap 'kill $(jobs -p) 2>/dev/null || true' EXIT

jobname="${SYNORA_JOB:-debian}"
if [[ "${1:-}" == sync:archive:* ]]; then
	jobname="${1#sync:archive:}"
	jobname="${jobname//\/}"
	jobname="${jobname//.}"
elif [[ $# -ge 1 ]]; then
	echo "Invalid command line: $*" >&2
	exit 1
fi

storage="${SYNORA_STORAGE:-}"
if [[ -z "$storage" ]]; then
	echo "SYNORA_STORAGE is required" >&2
	exit 1
fi

upstream="${SYNORA_UPSTREAM:-}"
rsync_host=""
rsync_path=""
if [[ "$upstream" == rsync://* ]]; then
	rest="${upstream#rsync://}"
	rsync_host="${rest%%/*}"
	rsync_path="${rest#*/}"
	rsync_path="${rsync_path#/}"
fi
if [[ -z "$rsync_host" ]]; then
	echo "SYNORA_UPSTREAM must be rsync://host/path (got: ${upstream:-empty})" >&2
	exit 1
fi

mkdir -p "$FTPSYNC_LOG_DIR" "$FTPSYNC_ETC" "$storage"
# Empty leftover dest-named directory (job name) makes rsync --delete
# return 23 (rmdir: Device or resource busy) on some layouts.
nested="$storage/$jobname"
if [[ -d "$nested" ]]; then
	rmdir "$nested" 2>/dev/null || true
fi
# Killed runs leave archvsync lock files; a new run must not treat those
# as "already running".
rm -f "$storage"/Archive-Update-in-Progress-* || true

mirrorname="${MIRRORNAME:-$(hostname -f 2>/dev/null || hostname)}"
conf="${FTPSYNC_ETC}/ftpsync-${jobname}.conf"
cat > "$conf" <<CONF
MIRRORNAME="${mirrorname}"
TO="${storage}"
RSYNC_HOST="${rsync_host}"
RSYNC_PATH="${rsync_path}"
MAILTO=""
ERRORMAILTO=""
LOGDIR="${FTPSYNC_LOG_DIR}"
SLEEP="${SLEEP}"
CONF

if [[ ! -x "$(command -v "$FTPSYNC" || true)" ]]; then
	echo "ftpsync not found" >&2
	exit 1
fi

"${FTPSYNC}" "sync:archive:${jobname}" &
PID=$!
sleep 2
if [[ ! -f "${FTPSYNC_LOG_DIR}/ftpsync-${jobname}.log" ]]; then
	echo "Failed to start ftpsync, please check configuration file." >&2
	exit 1
fi
tail --retry -n 0 -f "${FTPSYNC_LOG_DIR}/ftpsync-${jobname}.log" &
tail --retry -n 0 -f "${FTPSYNC_LOG_DIR}/rsync-ftpsync-${jobname}.log" &
tail --retry -n 0 -f "${FTPSYNC_LOG_DIR}/rsync-ftpsync-${jobname}.error" &
set +e
wait "$PID"
rc=$?
set -e

sz=""
for log in "${FTPSYNC_LOG_DIR}/rsync-ftpsync-${jobname}.log.0" "${FTPSYNC_LOG_DIR}/rsync-ftpsync-${jobname}.log"; do
	if [[ -f "$log" ]]; then
		sz=$(grep -Po '(?<=Total file size: )\d+' "$log" | tail -n 1 || true)
		[[ -n "$sz" ]] && break
	fi
done
if [[ -n "$sz" ]]; then
	echo "Total size is $(numfmt --to=iec "$sz")"
	echo "SYNORA_SIZE=${sz}"
fi

if [[ "$rc" -ne 0 ]]; then
	echo "ftpsync exited with ${rc}" >&2
	echo "SYNORA_STATUS=failed" >&2
	exit "$rc"
fi
echo "SYNORA_STATUS=success"
