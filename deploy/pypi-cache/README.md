# PyPI index and popularity cache

The `pypi` Synora job runs on `worker-nvme` (172.31.32.150). Shadowmire
maintains the complete index, including links to uncached packages, then Yukina
downloads popular packages from seven days of access logs. Sources are pinned in
the Dockerfile. The upstream is `https://mirrors.tuna.tsinghua.edu.cn/pypi/web/`.

## Storage and runtime

Create `data/pypi`, mounted at `/data/pypi`, with compression=lz4, atime=off,
exec=off, setuid=off, devices=off and sync=standard. Do not impose a 512 GiB
dataset quota: **549755813888 bytes is the package-cache budget**, while indexes,
SQLite state and temporary downloads need additional space. ZFS usage is
reported through Synora's existing filesystem statistics without a directory walk.

Build with `docker build -t synora-pypi:production deploy/pypi-cache`.
If HTTPS downloads require an existing environment proxy, append
`--build-arg HTTPS_PROXY --build-arg https_proxy`; do not put credentials in the
Dockerfile. Set `image` in the deployed job to the immutable local image ID from
`docker image inspect --format '{{.Id}}' synora-pypi:production`.

The image mounts the mirror at `/data`, dedicated JSON access logs at
`/nginx-log:ro`, and optional historical logs below `/nginx-legacy:ro`. Historical
NYIST logs contain a content-type column that ordinary combined-log parsers do
not understand. The converter handles that layout and gzip rotations, canonicalizes
legacy `/pypi/web/` URLs and refuses invalid or absent recent log input before GC.
Legacy directories must be real bind mounts; recursive discovery does not follow
directory symlinks. Retire the historical mounts after seven days of dedicated logs.

`PYPI_CACHE_BYTES` defaults to 512 GiB, `SHADOWMIRE_WORKERS` to 16, and
`PYPI_UPSTREAM` to the URL above. State under `.synora/` includes size databases,
validated logs and atomic success markers; the Nginx allowlist makes it private.
An initial success requires both the complete index update and a real cached
package. Interrupted updates resume from Shadowmire's serial database.

## Deployment and promotion

1. Back up both existing PyPI virtual-host configurations. Install this directory
   at `/opt/synora/pypi-cache`, and install the Nginx artifacts following
   [nginx/README.md](nginx/README.md). Initially include the server snippet only in
   a private validation server listening on `127.0.0.1:18081`; public routes keep
   their existing TUNA behavior until the complete initial sync passes validation.
2. Install `logrotate` as `/etc/logrotate.d/synora-pypi`, adjusting its `nginx` user
   only if the host uses a different Nginx account. Preserve the existing site's
   security policy and log pipelines. New package misses return 302 to TUNA; local
   cache hits remain local for campus and external clients alike.
3. Install `pypi.toml` as `/etc/synora/jobs/pypi.toml` with the actual immutable
   image ID. Run `synora -c /etc/synora/synora.toml check`, then `reload` and
   trigger `synora -c /etc/synora/synora.toml run pypi` if it is not already queued.
4. **All scheduling belongs to Synora.** Its five-minute interval skips overlapping
   ticks during the long bootstrap. No systemd timer or cron job is installed.
   The success hook executes `activate.py`: it checks the success marker, validates
   private HTTP behavior and an actual pip download, switches both public sites,
   runs `nginx -t`, reloads, and verifies both hostnames. It writes `activated.json`
   only after those checks pass. Subsequent success hooks then do nothing.
5. A failed promotion restores both previous routes. Synora retries promotion
   after a later successful sync. The hook has a bounded deadline within Synora's
   30-second command timeout. Inspect the worker journal for hook failures and
   `.synora/activated.json` to distinguish synchronized data from activated serving.

The activation script targets the two existing production virtual hosts
`mirror.nyist.edu.cn` and `mirrors.ha.edu.cn`. It deliberately rejects an unexpected
old location layout. Backups are stored under `/root/synora-pypi-activation-*`.
To roll back serving, pause the PyPI job before restoring the saved site files,
validate and reload Nginx, and retain the dataset for recovery.

## Verification

```sh
python3 -m unittest discover -s deploy/pypi-cache -p 'test_*.py'
node deploy/pypi-cache/nginx/test_nginx.mjs
python3 deploy/pypi-cache/nginx/test_nginx.py
python3 deploy/pypi-cache/nginx/verify_nginx.py \
  --base-url http://127.0.0.1:18081 --storage /data/pypi
```

The Docker Nginx integration test uses a small fixture, including real HEAD/Range
requests, negotiation, canonical names, fixed-origin cache misses and hidden
state. The final command uses real mirrored files and compares a downloaded
range's hash with the local package. `scripts/ci.sh` includes the offline runtime
and Nginx tests; image construction is separately verified before deployment.
