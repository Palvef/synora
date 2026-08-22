#!/usr/bin/env python3
"""Optional local HTTP proxy that always CONNECTs to the parent, then execs.

Not the image PID 1. The worker injects HTTP_PROXY/ALL_PROXY and runs the
job under docker --init. Keep this helper for tools that cannot CONNECT
plain `http://` URLs through a CONNECT-only parent (manager expose).
If HTTP_PROXY/ALL_PROXY is not an http(s) URL, the child is exec'd as-is.
"""

from __future__ import annotations

import base64
import os
import select
import signal
import socket
import subprocess
import sys
import threading
import urllib.parse


def parent_proxy() -> str | None:
    for key in (
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ):
        value = os.environ.get(key, "").strip()
        if value.lower().startswith("http://") or value.lower().startswith("https://"):
            return value
    return None


def parse_proxy(url: str) -> tuple[str, int, str | None, str | None]:
    parsed = urllib.parse.urlparse(url)
    if not parsed.hostname:
        raise ValueError(f"invalid proxy URL: {url}")
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    return parsed.hostname, port, parsed.username, parsed.password


def connect_parent(
    proxy_host: str,
    proxy_port: int,
    host: str,
    port: int,
    user: str | None,
    password: str | None,
) -> socket.socket:
    sock = socket.create_connection((proxy_host, proxy_port), timeout=60)
    req = f"CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n"
    if user is not None:
        token = base64.b64encode(f"{user}:{password or ''}".encode()).decode()
        req += f"Proxy-Authorization: Basic {token}\r\n"
    req += "\r\n"
    sock.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf and len(buf) < 65536:
        chunk = sock.recv(4096)
        if not chunk:
            break
        buf += chunk
    status = buf.split(b"\r\n", 1)[0]
    if b" 200 " not in status:
        sock.close()
        raise OSError(f"CONNECT {host}:{port} failed: {status!r}")
    return sock


def splice(left: socket.socket, right: socket.socket) -> None:
    try:
        while True:
            readable, _, _ = select.select([left, right], [], [], 120)
            if not readable:
                continue
            for src in readable:
                dst = right if src is left else left
                data = src.recv(65536)
                if not data:
                    return
                dst.sendall(data)
    except Exception:
        return
    finally:
        for sock in (left, right):
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except Exception:
                pass
            try:
                sock.close()
            except Exception:
                pass


def _read_headers(conn: socket.socket, rest: bytes) -> bytes:
    while b"\r\n\r\n" not in rest:
        chunk = conn.recv(4096)
        if not chunk:
            break
        rest += chunk
        if len(rest) > 1024 * 1024:
            break
    return rest


def handle_client(
    conn: socket.socket,
    proxy_host: str,
    proxy_port: int,
    user: str | None,
    password: str | None,
) -> None:
    try:
        conn.settimeout(120)
        buf = b""
        while b"\r\n" not in buf:
            chunk = conn.recv(4096)
            if not chunk:
                conn.close()
                return
            buf += chunk
        first, rest = buf.split(b"\r\n", 1)
        parts = first.decode("latin1").split(" ")
        if len(parts) < 2:
            conn.close()
            return
        method, target = parts[0], parts[1]
        rest = _read_headers(conn, rest)
        if method.upper() == "CONNECT":
            host, port_s = target.rsplit(":", 1)
            upstream = connect_parent(
                proxy_host, proxy_port, host, int(port_s), user, password
            )
            conn.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            splice(conn, upstream)
            return
        if target.startswith("http://") or target.startswith("https://"):
            parsed = urllib.parse.urlparse(target)
            host = parsed.hostname or ""
            port = parsed.port or (443 if parsed.scheme == "https" else 80)
            path = parsed.path or "/"
            if parsed.query:
                path += "?" + parsed.query
            header_blob, body = rest.split(b"\r\n\r\n", 1) if b"\r\n\r\n" in rest else (rest, b"")
            kept = [
                line
                for line in header_blob.split(b"\r\n")
                if line and not line.lower().startswith(b"proxy-")
            ]
            origin = (
                f"{method} {path} HTTP/1.1\r\n".encode()
                + b"\r\n".join(kept)
                + b"\r\n\r\n"
                + body
            )
            upstream = connect_parent(
                proxy_host, proxy_port, host, port, user, password
            )
            upstream.sendall(origin)
            splice(conn, upstream)
            return
        conn.sendall(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
        conn.close()
    except Exception:
        try:
            conn.close()
        except Exception:
            pass


def start_local(parent_url: str) -> int:
    proxy_host, proxy_port, user, password = parse_proxy(parent_url)
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(128)
    port = server.getsockname()[1]

    def loop() -> None:
        while True:
            client, _ = server.accept()
            threading.Thread(
                target=handle_client,
                args=(client, proxy_host, proxy_port, user, password),
                daemon=True,
            ).start()

    threading.Thread(target=loop, daemon=True).start()
    return port


def main() -> None:
    if "--" in sys.argv:
        child = sys.argv[sys.argv.index("--") + 1 :]
    else:
        child = sys.argv[1:]
    if not child:
        sys.stderr.write("usage: http_connect_proxy.py -- command...\n")
        sys.exit(2)
    parent = parent_proxy()
    if parent:
        port = start_local(parent)
        local = f"http://127.0.0.1:{port}"
        for key in (
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ):
            os.environ[key] = local
        extra = "127.0.0.1,localhost,::1"
        existing = os.environ.get("NO_PROXY", os.environ.get("no_proxy", "")).strip()
        os.environ["NO_PROXY"] = f"{existing},{extra}" if existing else extra
        os.environ["no_proxy"] = os.environ["NO_PROXY"]
    # Stay resident: exec would kill the local CONNECT thread.
    proc = subprocess.Popen(child)
    def _forward(signum, _frame):
        try:
            proc.send_signal(signum)
        except Exception:
            pass
    signal.signal(signal.SIGTERM, _forward)
    signal.signal(signal.SIGINT, _forward)
    sys.exit(proc.wait())


if __name__ == "__main__":
    main()
