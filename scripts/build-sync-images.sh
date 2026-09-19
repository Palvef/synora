#!/bin/bash
# Build independent job runtimes. Artifact source images must exist locally or in a registry.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TAG=${TAG:-latest}
SOURCE_RUNTIME=${SOURCE_RUNTIME:-synora-scripts:0.2.0}
roles=("$@")
if [ ${#roles[@]} -eq 0 ]; then roles=(scripts rubygems rustup nix-channels yukina shadowmire ftpsync); fi
for role in "${roles[@]}"; do
  case "$role" in scripts|rubygems|rustup|nix-channels|yukina|shadowmire|ftpsync) ;; *) echo "Unknown role: $role" >&2; exit 2;; esac
  file="$ROOT/deploy/docker/synora-$role/Dockerfile"
  if [ "$role" = scripts ]; then file="$ROOT/synora-scripts/Dockerfile"; fi
  args=(--network host --build-arg "SOURCE_RUNTIME=$SOURCE_RUNTIME")
  if [ "$role" = yukina ] || [ "$role" = shadowmire ]; then
    args+=(--build-arg "PYPI_RUNTIME=${PYPI_RUNTIME:?set PYPI_RUNTIME to the source-pinned cache runtime image}")
  fi
  if [ -n "${FETCH_HTTPS_PROXY:-}" ]; then args+=(--build-arg "FETCH_HTTPS_PROXY=$FETCH_HTTPS_PROXY"); fi
  docker build "${args[@]}" -t "synora-$role:$TAG" -f "$file" "$ROOT/synora-scripts"
done
