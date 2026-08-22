#!/bin/bash
set -euo pipefail

cd "$SYNORA_STORAGE"
echo "==== SYNC rustup START ===="
echo "proxy env before sidecar:"
env | grep -i proxy || echo "(none)"

# Sidecar must fetch official through the manager-assigned proxy.
# rustup-mirror must NOT inherit it: otherwise GET 127.0.0.1 is sent to
# the CONNECT expose and comes back 405.
SIDECAR_ALL_PROXY="${ALL_PROXY:-${all_proxy:-${HTTPS_PROXY:-${HTTP_PROXY:-}}}}"

OFFICIAL="${SYNORA_UPSTREAM:-https://static.rust-lang.org/}"
BASE_URL=${MIRROR_BASE_URL:-${SYNORA_MIRROR_BASE_URL:-}}
GC=${RUSTUP_GC:-"30"}
if [ -z "$BASE_URL" ]; then
  echo "MIRROR_BASE_URL is required (public URL written into rustup manifests)" >&2
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
  echo "python3/curl missing; python3/curl required in synora-scripts image" >&2
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi
MIRROR_BIN=/usr/local/cargo/bin/rustup-mirror
if [ ! -x "$MIRROR_BIN" ]; then
  MIRROR_BIN=/usr/lib/synora/scripts/rustup-mirror
fi
if [ ! -x "$MIRROR_BIN" ]; then
  MIRROR_BIN=/home/tunasync-scripts/rustup-mirror
fi
if [ ! -x "$MIRROR_BIN" ]; then
  echo "rustup-mirror binary not found" >&2
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi
echo "rustup-mirror binary: $MIRROR_BIN"

portfile=$(mktemp)
export RUSTUP_PROXY_PORTFILE="$portfile"

echo "official upstream: ${OFFICIAL}"
echo "manifest public url (-u): ${BASE_URL}"
echo "sidecar ALL_PROXY: ${SIDECAR_ALL_PROXY:-none}"

PROXY_PY=/usr/lib/synora/scripts/rustup-official-proxy.py
if [ ! -f "$PROXY_PY" ]; then
  PROXY_PY=/home/tunasync-scripts/rustup-official-proxy.py
fi
if [ ! -f "$PROXY_PY" ]; then
  echo "rustup-official-proxy.py not found" >&2
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi

ALL_PROXY="$SIDECAR_ALL_PROXY" all_proxy="$SIDECAR_ALL_PROXY" \
  HTTP_PROXY="$SIDECAR_ALL_PROXY" HTTPS_PROXY="$SIDECAR_ALL_PROXY" \
  http_proxy="$SIDECAR_ALL_PROXY" https_proxy="$SIDECAR_ALL_PROXY" \
  python3 "$PROXY_PY" "$OFFICIAL" &
proxy_pid=$!
trap 'kill "$proxy_pid" 2>/dev/null || true; rm -f "$portfile"' EXIT

for _ in $(seq 1 50); do
  if [ -s "$portfile" ]; then
    break
  fi
  sleep 0.1
done
if [ ! -s "$portfile" ]; then
  echo "local official proxy failed to start" >&2
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi
PORT=$(cat "$portfile")
echo "local official proxy http://127.0.0.1:${PORT}/ -> ${OFFICIAL}"

unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy FTP_PROXY ftp_proxy NO_PROXY no_proxy || true

echo "proxy probe:"
ok=0
for attempt in 1 2 3 4 5; do
  if curl --noproxy '*' -4 -sS -D /tmp/rustup-probe.hdr -o /tmp/rustup-probe.toml -w "probe attempt=${attempt} bytes=%{size_download} code=%{http_code}\n" --connect-timeout 15 --max-time 180 "http://127.0.0.1:${PORT}/dist/channel-rust-stable.toml"; then
    if grep -q '\[pkg.rust\]' /tmp/rustup-probe.toml; then
      ok=1
      break
    fi
  fi
  echo "probe attempt ${attempt} failed, retrying"
  sleep 2
done
head -n 20 /tmp/rustup-probe.hdr || true
wc -c /tmp/rustup-probe.toml
if [ "$ok" -ne 1 ]; then
  echo "official channel toml probe failed (not a rustup manifest)" >&2
  head -c 400 /tmp/rustup-probe.toml >&2 || true
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi
if ! grep -q '^\[pkg\.rust\]' /tmp/rustup-probe.toml && ! grep -q '\[pkg.rust\]' /tmp/rustup-probe.toml; then
  echo "official channel toml probe failed (not a rustup manifest)" >&2
  head -c 400 /tmp/rustup-probe.toml >&2 || true
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi
if grep -qi 'mirrors.tuna.tsinghua.edu.cn' /tmp/rustup-probe.toml; then
  echo "official channel toml still mentions tuna; refusing to continue" >&2
  echo "==== SYNC rustup FAILED ====" >&2
  echo "SYNORA_STATUS=failed" >&2
  exit 1
fi

"$MIRROR_BIN" -u "${BASE_URL}" -U "http://127.0.0.1:${PORT}/" -m "${SYNORA_STORAGE}" --gc "${GC}"
echo "==== SYNC rustup DONE ===="
echo "SYNORA_STATUS=success"
echo "finished"
