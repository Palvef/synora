# Serving the PyPI cache

This configuration follows the Shadowmire layout and the modern PyPI serving
requirements described in [Harry Chen's article](https://harrychen.xyz/2025/11/26/nginx-conf-to-serve-modern-pypi/).
Name normalization also collapses repeated separators, and content negotiation
honors Accept quality weights and excludes q=0 representations.

Install `synora-pypi.js` at `/etc/nginx/njs/synora-pypi.js`, install
`common-static.conf` at `/etc/nginx/snippets/synora-pypi-common.conf`, include
`http.conf` once in `http`, and include `server.conf` inside each public mirror
server after removing its previous PyPI locations. The njs HTTP module must be
loaded, along with fancyindex. Install `directory.conf` as
`/etc/nginx/snippets/synora-pypi-directory.conf`; `/pypi` redirects only to
`/pypi/`, which uses the site's existing fancyindex appearance and help links.
Only `simple`, `packages`, and `json` appear in that listing. Create `/var/log/nginx/pypi-cache` with permissions suitable for Nginx
logging. The existing site's `$is_forbidden` variable and `@forbidden` handler
are required; unrelated access policies remain in the enclosing server.
If using `conf.d`, install the HTTP definitions as `00-synora-pypi.conf` so the
log format is defined before the staging server is parsed.

Storage is explicitly rooted at `/data` and only the allowlisted PyPI paths are
served. Indexes use `Vary: Accept`; selected static files retain native Nginx
HEAD, conditional requests and byte-range behavior. Metadata misses return 404;
only valid package blob misses redirect to the fixed TUNA upstream. State files
are never served; only the public root has a directory listing. Dedicated JSON logs count local static
responses as `proxied: "0"` and package-miss redirects as `proxied: "1"`.

For staging, use a loopback-only server with the same server snippet and the
existing policy definitions. Validate before switching public routes:

```sh
node deploy/pypi-cache/nginx/test_nginx.mjs
python3 deploy/pypi-cache/nginx/test_nginx.py
python3 deploy/pypi-cache/nginx/verify_nginx.py \
  --base-url http://127.0.0.1:18081 --host mirror.nyist.edu.cn --storage /data/pypi
```

The Docker integration test uses `nginx:1.29-alpine` by default, overridable with
`PYPI_NGINX_TEST_IMAGE`. The production smoke verifier only reads data and HTTP
responses; it does not install packages, modify the cache or follow external
redirects. It requires a complete pip index and at least one cached package.
