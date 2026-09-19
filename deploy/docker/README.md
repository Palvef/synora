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

Build using `scripts/build-sync-images.sh [roles...]`; `TAG` defaults to `latest`.
The general, rustup and ftpsync images extract existing vetted artifacts from
`SOURCE_RUNTIME` (default `synora-scripts:0.2.0`). This source must be built/pulled
before the split. `PYPI_RUNTIME` is required for Yukina/Shadowmire: use the
source-pinned, production-patched PyPI build, preferably by digest. Deployments
should record the source and resulting image IDs and use immutable release tags.
The split copies only each specialized tool, not the source image filesystem.

Yukina's runner expects `PYPI_INDEX_MODE=proxy`; a full index sync belongs to the
separate Shadowmire image. Cache size and schedule remain job configuration.
Production PyPI build context/configuration stays outside Git in deploy/pypi-cache.

`FETCH_HTTPS_PROXY` optionally supplies build fetch proxy configuration. Do not
commit credentials or publish build logs containing proxy URLs.
