#!/usr/bin/env python3
import os
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

upstream = sys.argv[1].rstrip("/") if len(sys.argv) > 1 else ""
portfile = os.environ.get("RUSTUP_PROXY_PORTFILE", "")
proxy_url = (
    os.environ.get("ALL_PROXY")
    or os.environ.get("all_proxy")
    or os.environ.get("HTTPS_PROXY")
    or os.environ.get("HTTP_PROXY")
    or ""
)
CACHE = {}
CACHE_LOCK = threading.Lock()
CACHE_MAX = 8 * 1024 * 1024


def curl_config_value(value: str) -> str:
    """Quote one value for curl's config-file syntax.

    Request paths are remote-controlled, so they must never be interpolated
    into curl's argv. Reject control characters and quote the two characters
    that have meaning inside a double-quoted curl config value.
    """
    if any(ord(char) < 0x20 or ord(char) == 0x7F for char in value):
        raise ValueError("curl config value contains a control character")
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def map_official_path(path: str) -> str:
    """Map rustup-mirror request paths onto static.rust-lang.org.

    Toolchain files live under /dist/. rustup-init and
    release-stable.toml live under /rustup/. TUNA flattens that to
    /rustup/dist + /dist; official does not.
    """
    if "?" in path:
        path = path.split("?", 1)[0]
    if not path.startswith("/"):
        path = "/" + path
    if path.startswith("/rustup/"):
        return path
    if path == "/release-stable.toml":
        return "/rustup/release-stable.toml"
    parts = path.split("/")
    if (
        len(parts) == 4
        and parts[1] == "dist"
        and parts[3].startswith("rustup-init")
    ):
        return "/rustup" + path
    return path


def fetch(path: str) -> bytes:
    mapped = map_official_path(path)
    url = upstream + mapped
    env = {k: v for k, v in os.environ.items() if "proxy" not in k.lower()}
    last = None
    for attempt in range(1, 7):
        fd, out = tempfile.mkstemp(prefix="rustup-fetch-")
        os.close(fd)
        config_fd, config_path = tempfile.mkstemp(prefix="rustup-curl-", text=True)
        try:
            with os.fdopen(config_fd, "w", encoding="utf-8") as config:
                config.write("url = %s\n" % curl_config_value(url))
                config.write("output = %s\n" % curl_config_value(out))
                if proxy_url:
                    config.write("proxy = %s\n" % curl_config_value(proxy_url))
            cmd = [
                "/usr/bin/curl",
                "-fsS",
                "-4",
                "--http1.1",
                "--connect-timeout",
                "15",
                "--max-time",
                "1800",
                "--speed-limit",
                "50",
                "--speed-time",
                "120",
                "-A",
                "synora-rustup",
                "--config",
                config_path,
            ]
            sys.stderr.write("sidecar curl %s attempt %s\n" % (url, attempt))
            proc = subprocess.run(cmd, stderr=subprocess.PIPE, env=env)
            err = proc.stderr.decode("utf-8", "replace").strip()
            if proc.returncode == 0:
                with open(out, "rb") as fh:
                    data = fh.read()
                if data:
                    sys.stderr.write(
                        "sidecar fetch %s -> %s %d bytes\n"
                        % (path, mapped, len(data))
                    )
                    return data
                last = RuntimeError("empty body")
            else:
                last = RuntimeError("curl rc=%s %s" % (proc.returncode, err[:400]))
        except Exception as exc:
            last = exc
        finally:
            try:
                os.unlink(out)
            except OSError:
                pass
            try:
                os.unlink(config_path)
            except OSError:
                pass
        sys.stderr.write("sidecar fetch %s attempt %s: %s\n" % (path, attempt, last))
    raise last


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_GET(self):
        try:
            with CACHE_LOCK:
                cached = CACHE.get(self.path)
            if cached is None:
                data = fetch(self.path)
                if len(data) <= CACHE_MAX:
                    with CACHE_LOCK:
                        CACHE[self.path] = data
            else:
                data = cached
        except Exception as exc:
            # rustup-mirror treats a Content-Length as success and will
            # parse the error body as TOML. Send 502 with no length.
            sys.stderr.write("sidecar fetch %s failed: %s\n" % (self.path, exc))
            self.send_response(502)
            self.send_header("Connection", "close")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        sys.stderr.write("sidecar %s\n" % (fmt % args))


def _selftest():
    assert curl_config_value('https://example.invalid/a\\b"c') == (
        '"https://example.invalid/a\\\\b\\"c"'
    )
    try:
        curl_config_value("https://example.invalid/a\nnext-option")
    except ValueError:
        pass
    else:
        raise AssertionError("curl config control characters must be rejected")
    assert map_official_path("/dist/channel-rust-stable.toml") == (
        "/dist/channel-rust-stable.toml"
    )
    assert map_official_path("/rustup/dist/x86_64-unknown-linux-gnu/rustup-init") == (
        "/rustup/dist/x86_64-unknown-linux-gnu/rustup-init"
    )
    assert map_official_path("/dist/x86_64-unknown-linux-gnu/rustup-init") == (
        "/rustup/dist/x86_64-unknown-linux-gnu/rustup-init"
    )
    assert map_official_path("/dist/x86_64-pc-windows-gnu/rustup-init.exe") == (
        "/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe"
    )
    assert map_official_path("/rustup/release-stable.toml") == (
        "/rustup/release-stable.toml"
    )
    assert map_official_path("/release-stable.toml") == "/rustup/release-stable.toml"
    assert map_official_path(
        "/rustup/archive/1.27.1/x86_64-unknown-linux-gnu/rustup-init"
    ) == "/rustup/archive/1.27.1/x86_64-unknown-linux-gnu/rustup-init"
    print("rustup-official-proxy selftest ok")


if os.environ.get("RUSTUP_PROXY_SELFTEST"):
    _selftest()
    sys.exit(0)

if not upstream or not portfile:
    sys.stderr.write("usage: rustup-official-proxy.py <upstream>\n")
    sys.exit(2)

httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(portfile, "w", encoding="utf-8") as fh:
    fh.write(str(httpd.server_address[1]))
sys.stderr.write(
    "official sidecar listening on 127.0.0.1:%s -> %s proxy=%s\n"
    % (httpd.server_address[1], upstream, "configured" if proxy_url else "direct")
)
httpd.serve_forever()
