#!/usr/bin/env python3
import hashlib
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen

upstream = sys.argv[1].rstrip("/")
portfile = os.environ["RUSTUP_PROXY_PORTFILE"]


def fetch(path: str) -> bytes:
    if path.startswith("/rustup/"):
        path = path[len("/rustup") :]
    req = Request(upstream + path, headers={"User-Agent": "synora-rustup"})
    with urlopen(req, timeout=180) as resp:
        return resp.read()


def patch_toml(data: bytes) -> bytes:
    text = data.decode("utf-8", "replace")
    text = text.replace(
        "https://mirrors.tuna.tsinghua.edu.cn/rustup/dist/",
        "https://static.rust-lang.org/dist/",
    )
    text = text.replace(
        "https://mirrors.tuna.tsinghua.edu.cn/rustup/",
        "https://static.rust-lang.org/",
    )
    lines = []
    has_url = False
    has_hash = False
    for line in text.splitlines(True):
        if line.startswith("[") or line.startswith("[["):
            has_url = False
            has_hash = False
        stripped = line.lstrip()
        if stripped.startswith("url ") or stripped.startswith("url="):
            has_url = True
        if stripped.startswith("hash ") or stripped.startswith("hash="):
            has_hash = True
        lines.append(line)
        if stripped.startswith("xz_url") and not has_url:
            lines.append(line.replace("xz_url", "url", 1))
            has_url = True
        elif stripped.startswith("xz_hash") and not has_hash:
            lines.append(line.replace("xz_hash", "hash", 1))
            has_hash = True
    return "".join(lines).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_GET(self):
        try:
            if "channel-rust-" in self.path and self.path.endswith(".toml.sha256"):
                toml_path = self.path[: -len(".sha256")]
                patched = patch_toml(fetch(toml_path))
                digest = hashlib.sha256(patched).hexdigest()
                name = toml_path.rsplit("/", 1)[-1]
                data = f"{digest}  {name}\n".encode()
                ctype = "text/plain"
            elif "channel-rust-" in self.path and self.path.endswith(".toml"):
                data = patch_toml(fetch(self.path))
                ctype = "application/octet-stream"
            else:
                data = fetch(self.path)
                ctype = "application/octet-stream"
        except Exception as exc:
            msg = str(exc).encode()
            self.send_response(502)
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)
            return
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args):
        return


httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(portfile, "w", encoding="utf-8") as fh:
    fh.write(str(httpd.server_address[1]))
httpd.serve_forever()
