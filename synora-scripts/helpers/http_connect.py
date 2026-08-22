"""Force HTTP CONNECT through HTTP_PROXY.

The manager CONNECT expose answers plain `GET http://host/path` with 405.
urllib3/requests only tunnel HTTPS by default; this makes HTTP URLs tunnel
too so apt-sync/yum-sync work behind that proxy.
"""

from __future__ import annotations


def enable() -> None:
    try:
        from urllib3.util import proxy as proxy_util
    except Exception:
        return
    proxy_util.connection_requires_http_tunnel = lambda *args, **kwargs: True
