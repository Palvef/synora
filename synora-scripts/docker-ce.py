#!/usr/bin/env python3
import os
import sys
import time
import threading
import queue
import traceback
import signal
import hashlib
from pathlib import Path
from email.utils import parsedate_to_datetime
import re

import requests
from html.parser import HTMLParser


class _AnchorParser(HTMLParser):
    """Collect visible text of <a> tags, matching pyquery `link.text`."""

    def __init__(self):
        super().__init__()
        self.links = []
        self._in_a = False
        self._chunks = []

    def handle_starttag(self, tag, attrs):
        if tag == "a":
            self._in_a = True
            self._chunks = []

    def handle_data(self, data):
        if self._in_a:
            self._chunks.append(data)

    def handle_endtag(self, tag):
        if tag == "a" and self._in_a:
            self.links.append("".join(self._chunks))
            self._in_a = False


def parse_anchor_texts(html: str):
    parser = _AnchorParser()
    parser.feed(html or "")
    return parser.links


BASE_URL = os.getenv("SYNORA_UPSTREAM", "https://d2h67oheeuigaw.cloudfront.net/")
WORKING_DIR = os.getenv("SYNORA_STORAGE")
SYNC_USER_AGENT = os.getenv(
    "SYNC_USER_AGENT",
    "Docker-ce Syncing Tool (https://github.com/tuna/tunasync-scripts)/1.0",
)

TIMEOUT_OPTION = (
    float(os.getenv("DOCKER_CE_CONNECT_TIMEOUT", "30")),
    float(os.getenv("DOCKER_CE_READ_TIMEOUT", "60")),
)
MAX_RETRIES = int(os.getenv("DOCKER_CE_RETRIES", "5"))
requests.utils.default_user_agent = lambda: SYNC_USER_AGENT
requests.adapters.DEFAULT_RETRIES = 3

REL_URL_RE = re.compile(r"https?:\/\/.+?\/(.+?)(\/index\.html)?$")


def http_get(url, **kwargs):
    """GET with retries. Timeouts no longer kill the whole sync."""
    kwargs.setdefault("timeout", TIMEOUT_OPTION)
    last = None
    for attempt in range(1, MAX_RETRIES + 1):
        try:
            return requests.get(url, **kwargs)
        except Exception as exc:
            last = exc
            print(
                f"warning: GET {url} failed ({attempt}/{MAX_RETRIES}): {exc}",
                flush=True,
            )
            if attempt < MAX_RETRIES:
                time.sleep(min(2 ** (attempt - 1), 20))
    raise last


class RemoteSite:
    def __init__(self, base_url=BASE_URL):
        if not base_url.endswith("/"):
            base_url = base_url + "/"
        self.base_url = base_url
        self.meta_urls = []
        self.incomplete = False

    def is_metafile_url(self, url):
        deb_dists = ("debian", "ubuntu", "raspbian")
        rpm_dists = ("fedora", "centos")

        for dist in deb_dists:
            if "/" + dist + "/" not in url:
                continue
            if "/Contents-" in url:
                return True
            if "/binary-" in url:
                return True
            if "Release" in url:
                return True

        for dist in rpm_dists:
            if "/" + dist + "/" not in url:
                continue
            if "/repodata/" in url:
                return True

        return False

    def recursive_get_filelist(self, base_url, filter_meta=False):
        if not base_url.endswith("/"):
            yield base_url
            return

        try:
            r = http_get(base_url)
            if r.url != base_url:
                target_dir = r.url.split("/")[-2]
                origin_dir = base_url.split("/")[-2]
                if target_dir != origin_dir:
                    from_dir = REL_URL_RE.findall(base_url)[0][0]
                    to_dir = REL_URL_RE.findall(r.url)[0][0]
                    yield (from_dir, to_dir)
                    return
        except Exception:
            print(f"warning: listing {base_url} failed, skip this directory", flush=True)
            traceback.print_exc()
            self.incomplete = True
            return
        if not r.ok:
            print(f"warning: listing {base_url} HTTP {r.status_code}, skip", flush=True)
            self.incomplete = True
            return

        for text in parse_anchor_texts(r.text):
            if not text or text.startswith(".."):
                continue
            href = base_url + text
            if filter_meta and self.is_metafile_url(href):
                self.meta_urls.append(href)
            elif text.endswith("/"):
                yield from self.recursive_get_filelist(href, filter_meta=filter_meta)
            else:
                yield href

    def relpath(self, url):
        assert url.startswith(self.base_url)
        return url[len(self.base_url) :]

    @property
    def files(self):
        yield from self.recursive_get_filelist(self.base_url, filter_meta=True)
        for url in self.meta_urls:
            yield from self.recursive_get_filelist(url, filter_meta=False)


def requests_download(remote_url: str, dst_file: Path):
    last = None
    for attempt in range(1, MAX_RETRIES + 1):
        try:
            with requests.get(remote_url, stream=True, timeout=TIMEOUT_OPTION) as r:
                r.raise_for_status()
                remote_ts = parsedate_to_datetime(
                    r.headers["last-modified"]
                ).timestamp()
                tmpfile = dst_file.parent / ("." + dst_file.name + ".tmp")
                with open(tmpfile, "wb") as f:
                    for chunk in r.iter_content(chunk_size=1024**2):
                        if chunk:
                            f.write(chunk)
                os.utime(tmpfile, (remote_ts, remote_ts))
                tmpfile.rename(dst_file)
                return
        except Exception as exc:
            last = exc
            print(
                f"warning: download {remote_url} failed ({attempt}/{MAX_RETRIES}): {exc}",
                flush=True,
            )
            if attempt < MAX_RETRIES:
                time.sleep(min(2 ** (attempt - 1), 20))
    raise last


def downloading_worker(q):
    while True:
        item = q.get()
        if item is None:
            break

        try:
            url, dst_file, working_dir = item
            if dst_file.is_file():
                print("checking", url, flush=True)
                r = None
                for attempt in range(1, MAX_RETRIES + 1):
                    try:
                        r = requests.head(
                            url, timeout=TIMEOUT_OPTION, allow_redirects=True
                        )
                        break
                    except Exception as exc:
                        print(
                            f"warning: HEAD {url} failed ({attempt}/{MAX_RETRIES}): {exc}",
                            flush=True,
                        )
                        if attempt < MAX_RETRIES:
                            time.sleep(min(2 ** (attempt - 1), 20))
                if r is None:
                    raise RuntimeError(f"HEAD failed after {MAX_RETRIES} retries")
                remote_filesize = int(r.headers["content-length"])
                remote_date = parsedate_to_datetime(r.headers["last-modified"])
                stat = dst_file.stat()
                local_filesize = stat.st_size
                local_mtime = stat.st_mtime

                if (
                    remote_filesize == local_filesize
                    and remote_date.timestamp() == local_mtime
                ):
                    print("skipping", dst_file.relative_to(working_dir), flush=True)
                    continue
                print(
                    "diff",
                    dst_file.relative_to(working_dir),
                    "remote",
                    remote_filesize,
                    remote_date,
                    "local",
                    local_filesize,
                    local_mtime,
                    flush=True,
                )
                if r.headers.get("etag") and remote_filesize == local_filesize:
                    remote_md5 = r.headers["etag"].strip('"')
                    if re.match(r"^[a-fA-F0-9]{32}$", remote_md5):
                        print(
                            "checking md5",
                            dst_file.relative_to(working_dir),
                            flush=True,
                        )
                        local_md5 = hashlib.md5(dst_file.read_bytes()).hexdigest()
                        if remote_md5.lower() == local_md5:
                            print(
                                "skipping (md5 match)",
                                dst_file.relative_to(working_dir),
                                flush=True,
                            )
                            os.utime(
                                dst_file,
                                (remote_date.timestamp(), remote_date.timestamp()),
                            )
                            continue
                dst_file.unlink()
            print("downloading", url, flush=True)
            requests_download(url, dst_file)
        except Exception:
            traceback.print_exc()
            print("Failed to download", url, flush=True)
            if dst_file.is_file():
                dst_file.unlink()
        finally:
            q.task_done()


def create_workers(n):
    task_queue = queue.Queue()
    for i in range(n):
        t = threading.Thread(target=downloading_worker, args=(task_queue,))
        t.start()
    return task_queue


def create_symlink(from_dir: Path, to_dir: Path):
    to_dir = to_dir.relative_to(from_dir.parent)
    if from_dir.exists():
        if from_dir.is_symlink():
            resolved_symlink = from_dir.resolve().relative_to(
                from_dir.parent.absolute()
            )
            if resolved_symlink != to_dir:
                print(
                    f"WARN: The symlink {from_dir} dest changed from {resolved_symlink} to {to_dir}."
                )
        else:
            print(
                f"WARN: The symlink {from_dir} exists on disk but it is not a symlink."
            )
    else:
        if from_dir.is_symlink():
            print(f"WARN: The symlink {from_dir} is probably invalid.")
        else:
            from_dir.parent.mkdir(parents=True, exist_ok=True)
            from_dir.symlink_to(to_dir)


def _on_signal(signum, _frame):
    print(f"==== SYNC docker-ce CANCELLED signal={signum} ====", flush=True)
    raise SystemExit(143)


def main():
    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGINT, _on_signal)
    print("==== SYNC docker-ce START upstream=%s ====" % BASE_URL, flush=True)
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default=BASE_URL)
    parser.add_argument("--working-dir", default=WORKING_DIR)
    parser.add_argument(
        "--workers",
        default=1,
        type=int,
        help="number of concurrent downloading jobs",
    )
    parser.add_argument(
        "--fast-skip",
        action="store_true",
        help="do not verify size and timestamp of existing package files",
    )
    args = parser.parse_args()

    if args.working_dir is None:
        raise Exception("Working Directory is None")

    working_dir = Path(args.working_dir)
    task_queue = create_workers(args.workers)

    remote_filelist = []
    rs = RemoteSite(args.base_url)
    for url in rs.files:
        if isinstance(url, tuple):
            from_dir, to_dir = url
            create_symlink(working_dir / from_dir, working_dir / to_dir)
        else:
            dst_file = working_dir / rs.relpath(url)
            remote_filelist.append(dst_file.relative_to(working_dir))

            if dst_file.is_file():
                if args.fast_skip and dst_file.suffix in [".rpm", ".deb", ".tgz", ".zip"]:
                    print(
                        "fast skipping",
                        dst_file.relative_to(working_dir),
                        flush=True,
                    )
                    continue
            else:
                dst_file.parent.mkdir(parents=True, exist_ok=True)

            task_queue.put((url, dst_file, working_dir))

    task_queue.join()
    for i in range(args.workers):
        task_queue.put(None)

    if rs.incomplete:
        print(
            "listing was incomplete; skipping deletes so local extras are kept",
            flush=True,
        )
        print("==== SYNC docker-ce FAILED: listing incomplete ====", flush=True)
        raise SystemExit(1)

    local_filelist = []
    for local_file in working_dir.glob("**/*"):
        if local_file.is_file():
            local_filelist.append(local_file.relative_to(working_dir))

    deleted = 0
    for old_file in set(local_filelist) - set(remote_filelist):
        print("deleting", old_file, flush=True)
        old_file = working_dir / old_file
        old_file.unlink()
        deleted += 1
    total = 0
    try:
        import subprocess
        out = subprocess.check_output(["du", "-sb", str(working_dir)], text=True)
        total = int(out.split()[0])
    except Exception:
        total = 0
    def iec(n):
        n = float(n)
        for unit in ("", "K", "M", "G", "T"):
            if n < 1024.0 or unit == "T":
                if not unit:
                    return str(int(n))
                v = f"{n:.1f}".rstrip("0").rstrip(".")
                return v + unit
            n /= 1024.0
        return str(int(n))
    print("Total size is", iec(total), flush=True)
    print(
        "==== SYNC docker-ce DONE files=%d deleted=%d ===="
        % (len(remote_filelist), deleted),
        flush=True,
    )


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception:
        traceback.print_exc()
        print("==== SYNC docker-ce FAILED ====", flush=True)
        raise SystemExit(1)
