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
