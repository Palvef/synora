# PyPI index and popularity cache

The `pypi` Synora job runs on `worker-nvme` (172.31.32.150). Production proxies
index metadata to TUNA and Yukina downloads popular packages from seven days of
access logs; optional full-index mode uses Shadowmire. Sources are pinned in
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

## Production cache mode

Production sets `PYPI_INDEX_MODE=proxy`. Nginx proxies normalized HTML/JSON
metadata to TUNA with certificate verification and serves cached package files
from `/data/pypi/packages`. Package misses return 302 to TUNA. Install
`nginx/server-cache.conf` as the server snippet and `nginx/proxy.conf` as
`/etc/nginx/snippets/synora-pypi-proxy.conf`. HTTP definitions must be loaded first
(`/etc/nginx/conf.d/00-synora-pypi.conf`). Both public sites can switch immediately;
there is no full-index readiness gate or activation hook in this mode. Install
`nginx/directory.conf` as `/etc/nginx/snippets/synora-pypi-directory.conf`
to retain the site's default fancyindex at `/pypi/`, alongside the JSON API
and existing help documentation. `/pypi` redirects only to `/pypi/`.

The single Synora job runs every five minutes without overlapping runs. It only
processes package candidates from seven days of access logs; an access record is
not necessarily a unique package. Valid blob requests, including browser downloads,
participate from one vote; directory requests are excluded by the blob path filter. No cron or systemd timer is used. Preserve
existing partial index files for recovery, but proxy mode does not serve them.

`PYPI_INDEX_MODE=full` remains available for a complete local Shadowmire index.
The original `nginx/server.conf` and optional `activate.py` hook support that
mode. Full-index synchronization must not block production cache population.

Runtime progress is summarized every 30 seconds instead of emitting one line per
project or an interactive progress bar. Synora retains the latest output when a
run log reaches 16 MiB, with an explicit truncation marker; it does not silently
stop recording a running task.

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
