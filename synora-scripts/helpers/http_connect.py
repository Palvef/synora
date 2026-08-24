"""Force HTTP CONNECT through HTTP_PROXY.

The manager CONNECT expose answers plain `GET http://host/path` with 405.
urllib3/requests only tunnel HTTPS by default; this makes HTTP URLs tunnel
too so apt-sync/yum-sync work behind that proxy.
"""

from __future__ import annotations


def enable() -> None:
    try:
        from urllib3 import connectionpool
        from urllib3.util import proxy as proxy_util
    except Exception:
        return
    always_tunnel = lambda *args, **kwargs: True
    # connectionpool imports this function by name, so replacing only the
    # original util module is too late once requests has imported urllib3.
    proxy_util.connection_requires_http_tunnel = always_tunnel
    connectionpool.connection_requires_http_tunnel = always_tunnel
