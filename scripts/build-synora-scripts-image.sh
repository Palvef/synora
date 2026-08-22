#!/bin/bash
# Build synora-scripts:latest from synora-scripts/Dockerfile.
# rustup-mirror is compiled in the image from jiegec/rustup-mirror.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${IMAGE:-synora-scripts:latest}
# HTTPS-only fetch proxy (git/cargo/gem/pip/curl). Apt stays direct:
# CONNECT exposes 405 HTTP GET.
PROXY=${FETCH_HTTPS_PROXY:-${HTTPS_PROXY:-${https_proxy:-}}}

usage() {
  cat <<'HELP'
Usage: scripts/build-synora-scripts-image.sh [--proxy URL] [--image NAME]

Builds the git/script runtime image used by synora-worker (git, python,
dnf, createrepo_c, awscli, ftpsync, rustup-mirror, rubygems-mirror).
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

echo "building $IMAGE from synora-scripts/Dockerfile"
BUILD_ARGS=(--network host -t "$IMAGE" -f "$ROOT/synora-scripts/Dockerfile" "$ROOT/synora-scripts")
if [ -n "$PROXY" ]; then
  echo "using HTTPS fetch proxy $PROXY (apt stays direct)"
  BUILD_ARGS+=(--build-arg "FETCH_HTTPS_PROXY=$PROXY")
fi
docker build "${BUILD_ARGS[@]}"
echo "built $IMAGE"
