# Independent synchronization images

| Image | Jobs / contents |
| --- | --- |
| synora-scripts | General scripts, APT/YUM, git, AOSP |
| synora-rubygems | Ruby and rubygems-mirror |
| synora-rustup | Patched rustup-mirror and official-upstream proxy |
| synora-nix-channels | Nix releases/channels scripts, AWS and MinIO clients |
| synora-yukina | Patched Yukina and access-log cache runner, proxy index mode |
| synora-shadowmire | Standalone Shadowmire CLI, no Yukina |
| synora-ftpsync | archvsync and Debian/Kali wrappers |

Images are published as `ghcr.io/palvef/synora-<role>:latest`. The
Synchronization images workflow builds all seven roles on relevant master pushes,
manual dispatches and version tags. It also publishes immutable commit tags and
version tags for releases. Production workers must be able to pull the packages;
public packages allow anonymous pulls.

Build locally with `scripts/build-sync-images.sh [roles...]`. `REGISTRY` defaults
to `ghcr.io/palvef` and `TAG` defaults to `latest`. All images build directly from source on GitHub-hosted runners. Tool source
revisions are pinned in the Dockerfiles. Rustup and Yukina retain their production
patches; Yukina's reusable runner is tracked in `synora-scripts/pypi-runtime`.
No prebuilt local image or production build context is uploaded or required.

Validate immutable rollout tags before assigning `latest`. Pull each updated image
on every worker before reloading job configuration. Keep previous digests for
rollback; changing a tag does not update already running containers.

Yukina's runner expects `PYPI_INDEX_MODE=proxy`; a full index sync belongs to the
separate Shadowmire image. Cache size and schedule remain job configuration.
Production PyPI deployment configuration stays outside Git in deploy/pypi-cache.

`FETCH_HTTPS_PROXY` optionally supplies build fetch proxy configuration. Do not
commit credentials or publish build logs containing proxy URLs.
