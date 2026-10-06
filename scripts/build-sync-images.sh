#!/bin/bash
# Build independent synchronization runtimes directly from pinned source.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TAG=${TAG:-latest}
REGISTRY=${REGISTRY:-ghcr.io/palvef}
roles=("$@")
if [ ${#roles[@]} -eq 0 ]; then roles=(scripts rubygems rustup nix-channels yukina shadowmire ftpsync); fi
for role in "${roles[@]}"; do
  case "$role" in scripts|rubygems|rustup|nix-channels|yukina|shadowmire|ftpsync) ;; *) echo "Unknown role: $role" >&2; exit 2;; esac
  file="$ROOT/deploy/docker/synora-$role/Dockerfile"
  if [ "$role" = scripts ]; then file="$ROOT/synora-scripts/Dockerfile"; fi
  args=(--network host)
  if [ -n "${FETCH_HTTPS_PROXY:-}" ]; then args+=(--build-arg "FETCH_HTTPS_PROXY=$FETCH_HTTPS_PROXY"); fi
  docker build "${args[@]}" --label org.opencontainers.image.source=https://github.com/Palvef/synora -t "$REGISTRY/synora-$role:$TAG" -f "$file" "$ROOT/synora-scripts"
done
