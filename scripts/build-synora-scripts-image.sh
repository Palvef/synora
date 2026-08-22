#!/bin/bash
# Build synora-scripts:latest on a worker. rustup-mirror is a local binary
# and is copied into the build context; it is not committed to git.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${IMAGE:-synora-scripts:latest}
# HTTPS-only fetch proxy (git/gem/pip/curl). Apt stays direct: CONNECT
# exposes 405 HTTP GET.
PROXY=${FETCH_HTTPS_PROXY:-${HTTPS_PROXY:-${https_proxy:-}}}
RUSTUP_MIRROR_SRC=${RUSTUP_MIRROR:-}

usage() {
  cat <<'HELP'
Usage: scripts/build-synora-scripts-image.sh [--proxy URL] [--image NAME]

Builds the git/script runtime image used by synora-worker (git, python,
dnf, createrepo_c, awscli, ftpsync, rustup-mirror, rubygems-mirror).
Looks for rustup-mirror in $RUSTUP_MIRROR, /usr/lib/synora/scripts,
or /home/tunasync-scripts.
HELP
}

while [ $# -gt 0 ]; do
  case "$1" in
    --proxy)
      PROXY=${2:?}
      shift 2
      ;;
    --image)
      IMAGE=${2:?}
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$RUSTUP_MIRROR_SRC" ]; then
  for p in /usr/lib/synora/scripts/rustup-mirror /home/tunasync-scripts/rustup-mirror; do
    if [ -x "$p" ]; then
      RUSTUP_MIRROR_SRC=$p
      break
    fi
  done
fi
if [ -z "$RUSTUP_MIRROR_SRC" ] || [ ! -x "$RUSTUP_MIRROR_SRC" ]; then
  echo "rustup-mirror binary not found (set RUSTUP_MIRROR=)" >&2
  exit 1
fi

CTX=$(mktemp -d)
trap 'rm -rf "$CTX"' EXIT
mkdir -p "$CTX/scripts"
cp -a "$ROOT/synora-scripts/." "$CTX/scripts/"
# check.sh / README are docs; keep them in the image for operators
rm -f "$CTX/scripts/check.sh"
rm -f "$CTX/scripts/rustup-mirror"
cp -a "$RUSTUP_MIRROR_SRC" "$CTX/rustup-mirror"
chmod 0755 "$CTX/rustup-mirror"
cp "$ROOT/deploy/docker/synora-scripts/Dockerfile" "$CTX/Dockerfile"
cat > "$CTX/.dockerignore" <<'IGNORE'
**/.git
**/*.pyc
**/__pycache__
**/.codesight
IGNORE

echo "building $IMAGE (rustup-mirror from $RUSTUP_MIRROR_SRC)"
BUILD_ARGS=(--network host)
if [ -n "$PROXY" ]; then
  echo "using HTTPS fetch proxy $PROXY (apt stays direct)"
  BUILD_ARGS+=(--build-arg "FETCH_HTTPS_PROXY=$PROXY")
fi
docker build "${BUILD_ARGS[@]}" -t "$IMAGE" "$CTX"
echo "built $IMAGE"
