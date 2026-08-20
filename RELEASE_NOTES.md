# Synora 0.1.4

## Fixes

- HTTP CONNECT expose copies both directions so TLS through WARP is not dropped
- Docker jobs inject ALL_PROXY and HTTP(S)_PROXY for HTTP CONNECT, matching tunasync; SOCKS still uses ALL_PROXY only
- Empty HTTP(S)_PROXY is no longer injected (reqwest treated that as "no proxy")
- Docker runs fail on tunasync script reports such as `Failed YUM repos` even if the process exited 0
- Docker `size_hint` reads `Total size is` / `size-sum:` / `SYNORA_SIZE=` from script output

## Improvements

- `docker_network` job field (`host`, `bridge`, `none`) for `docker run --network`
