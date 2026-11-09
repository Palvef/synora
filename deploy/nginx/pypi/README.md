# PyPI metadata cache

Include `metadata-http.conf` once in the Nginx `http` context, alongside the
existing PyPI njs variables. Include `metadata-proxy.conf` in the shared PyPI
proxy snippet. Create the cache directory writable by the Nginx worker user,
run `nginx -t`, then reload.

Successful Simple HTML/JSON indexes, JSON metadata and `.metadata` sidecars are
cached on demand for five minutes, with a 1 GiB disk limit. Keys include the njs
selected upstream path, so HTML and JSON responses stay separate. Failed
responses are never stored. Wheel and source distributions bypass this Nginx
cache and remain under Yukina's popularity-based package cache.

`X-PyPI-Metadata-Cache` reports MISS/HIT/STALE/BYPASS. Existing cache-control,
Vary and authorization handling remain enforced. Synchronization stays on the
Synora schedule; this cache requires no external timer.

Include `upstream-http.conf` once in the `http` context and set the shared PyPI
proxy snippet to `proxy_pass https://synora_pypi_tuna;`. Keep the upstream Host
and TLS SNI as `mirrors.tuna.tsinghua.edu.cn`. The upstream resolves AAAA records
through the local systemd-resolved stub (`127.0.0.53`); adjust that resolver on
hosts using another DNS service. This keeps cache misses on the same IPv6 path
as Yukina instead of intermittently receiving TUNA's IPv4 denial response.
