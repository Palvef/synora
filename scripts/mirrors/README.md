# Synora mirror scripts

Native `provider = "script"` helpers, ported from tunasync-scripts.

On a worker they run inside the `synora-scripts` image (see
`deploy/docker/synora-scripts/Dockerfile`). Job TOMLs keep
`provider = "script"` and `command = "/usr/lib/synora/scripts/..."`.
The image also contains git, so `provider = "git"` uses the same
runtime, plus ftpsync (archvsync) for `debian.sh` / `kali.sh`.
HTTP directory mirrors use Synora's native `http` provider, not
tsumugu in this image. Tunasync environment names
(`TUNASYNC_WORKING_DIR`, `TUNASYNC_UPSTREAM_URL`, proxy vars) are
injected by Synora.

Build on the worker:

```sh
scripts/build-synora-scripts-image.sh --proxy http://172.31.33.205:14000
```

Git-only jobs should use `provider = "git"` instead of `git.sh`.
