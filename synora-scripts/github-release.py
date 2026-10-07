#!/usr/bin/env python3
import concurrent.futures
import json
import logging
import os
import re
import fcntl
import hashlib
import time
from datetime import datetime
from pathlib import Path

import sys

import requests
import requests.utils

_HELPERS = Path(__file__).resolve().parent / "helpers"
if str(_HELPERS) not in sys.path:
    sys.path.insert(0, str(_HELPERS))
try:
    import http_connect
    http_connect.enable()
except Exception:
    pass

logger = logging.getLogger(__name__)
logger.setLevel(logging.INFO)
handler = logging.StreamHandler()
formatter = logging.Formatter(
    "%(asctime)s.%(msecs)03d - %(filename)s:%(lineno)d [%(levelname)s] %(message)s",
    datefmt="%Y-%m-%dT%H:%M:%S",
)
handler.setFormatter(formatter)
logger.addHandler(handler)

BASE_URL = os.getenv("SYNORA_UPSTREAM", "https://api.github.com/repos/")
WORKING_DIR = os.getenv("SYNORA_STORAGE")
# Job CWD is the storage root; default next to this script.
CONFIG = os.getenv(
    "GITHUB_RELEASE_CONFIG",
    str(Path(__file__).resolve().with_name("github-release.json")),
)
REPOS = []
UA = "hernet-github-release-mirror/0.0"

# connect and read timeout value
TIMEOUT_OPTION = (30, 60)


def sizeof_fmt(num: float, suffix: str = "iB") -> str:
    for unit in ["", "K", "M", "G", "T", "P", "E", "Z"]:
        if abs(num) < 1024.0:
            return "%3.2f%s%s" % (num, unit, suffix)
        num /= 1024.0
    return "%.2f%s%s" % (num, "Y", suffix)


# wrap around requests.get to use token if available
def github_get(*args, **kwargs) -> requests.Response:
    headers = kwargs["headers"] if "headers" in kwargs else {}
    if "GITHUB_TOKEN" in os.environ:
        headers["Authorization"] = "token {}".format(os.environ["GITHUB_TOKEN"])
    headers["User-Agent"] = UA
    kwargs["headers"] = headers
    kwargs["timeout"] = TIMEOUT_OPTION
    return requests.get(*args, **kwargs)


DOWNLOAD_ATTEMPTS = 5
RETRY_DELAY = 2
PROGRESS_INTERVAL = 30


class DownloadIntegrityError(ValueError):
    pass


def do_download(
    remote_url: str, dst_file: Path, remote_ts: float, remote_size: int,
    remote_digest: str | None = None,
) -> None:
    part = dst_file.with_name('.' + dst_file.name + '.synora-part')
    metadata = part.with_name(part.name + '.json')
    source = dict(url=remote_url, timestamp=remote_ts, size=remote_size, digest=remote_digest)
    lock_path = part.with_name(part.name + '.lock')
    with lock_path.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            state = json.loads(metadata.read_text())
        except (FileNotFoundError, ValueError):
            state = {}
        if not isinstance(state, dict) or state.get('source') != source:
            part.unlink(missing_ok=True)
            state = {'source': source}
        expected_digest = None
        if remote_digest:
            if not re.fullmatch(r'sha256:[0-9a-fA-F]{64}', remote_digest):
                raise DownloadIntegrityError('Unsupported asset digest: ' + remote_digest)
            expected_digest = remote_digest.split(':', 1)[1].lower()

        def save_state():
            temporary = metadata.with_name(metadata.name + '.new')
            temporary.write_text(json.dumps(state))
            temporary.replace(metadata)

        def publish():
            if expected_digest:
                with part.open('rb') as stream:
                    actual = hashlib.file_digest(stream, 'sha256').hexdigest()
                if actual != expected_digest:
                    part.unlink()
                    metadata.unlink(missing_ok=True)
                    raise DownloadIntegrityError(f'SHA-256 mismatch for {dst_file.name}')
            os.utime(part, (remote_ts, remote_ts))
            part.chmod(0o644)
            part.replace(dst_file)
            metadata.unlink(missing_ok=True)
            logger.info('downloaded %s (%s)', dst_file.name, sizeof_fmt(dst_file.stat().st_size))

        for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
            offset = part.stat().st_size if part.is_file() else 0
            if remote_size >= 0 and offset > remote_size:
                part.unlink()
                offset = 0
            if remote_size >= 0 and part.is_file() and offset == remote_size:
                publish()
                return
            # A validator (or an API digest) prevents mixing different object versions.
            if remote_size < 0 or not (state.get('validator') or expected_digest):
                offset = 0
            headers = {'Accept-Encoding': 'identity'}
            if offset:
                headers['Range'] = f'bytes={offset}-'
                if state.get('validator'):
                    headers['If-Range'] = state['validator']
                logger.info('resuming %s at %s/%s bytes (attempt %s/%s)', remote_url, offset, remote_size, attempt, DOWNLOAD_ATTEMPTS)
            try:
                with github_get(remote_url, stream=True, headers=headers) as response:
                    response.raise_for_status()
                    if response.headers.get('Content-Encoding', 'identity') != 'identity':
                        raise DownloadIntegrityError('Encoded response cannot be safely resumed')
                    etag = response.headers.get('ETag', '')
                    validator = etag if etag and not etag.startswith('W/') else response.headers.get('Last-Modified')
                    if response.status_code == 206:
                        content_range = re.fullmatch(r'bytes (\d+)-(\d+)/(\d+)', response.headers.get('Content-Range', ''))
                        if not content_range or int(content_range[1]) != offset or int(content_range[2]) < offset:
                            raise DownloadIntegrityError('Invalid Content-Range for resumed asset')
                        total = int(content_range[3])
                        if int(content_range[2]) >= total or (remote_size >= 0 and total != remote_size):
                            raise DownloadIntegrityError('Content-Range disagrees with GitHub asset size')
                        if offset and validator and state.get('validator') and validator != state['validator']:
                            raise DownloadIntegrityError('Asset validator changed during a ranged response')
                    elif response.status_code == 200:
                        # Range ignored or If-Range invalidated: replace, never append.
                        offset = 0
                    else:
                        raise DownloadIntegrityError(f'Unexpected download status: {response.status_code}')
                    state['validator'] = validator
                    started = last_report = time.monotonic()
                    received = 0
                    with part.open('ab' if offset else 'wb') as stream:
                        part.chmod(0o600)
                        save_state()
                        for chunk in response.iter_content(chunk_size=1 << 20):
                            if not chunk:
                                continue
                            stream.write(chunk)
                            received += len(chunk)
                            if remote_size >= 0 and offset + received > remote_size:
                                raise DownloadIntegrityError('Downloaded asset exceeds GitHub asset size')
                            now = time.monotonic()
                            if now-last_report >= PROGRESS_INTERVAL:
                                logger.info('download progress %s: %s/%s bytes, %s/s', dst_file.name, offset+received, remote_size if remote_size >= 0 else 'unknown', sizeof_fmt(received/max(now-started, 0.001)))
                                last_report = now
                    if remote_size >= 0 and part.stat().st_size != remote_size:
                        raise requests.ConnectionError(f'Incomplete asset: {part.stat().st_size}/{remote_size} bytes')
                publish()
                return
            except requests.RequestException as error:
                status = error.response.status_code if error.response is not None else None
                if status is not None and status not in (408, 429) and status < 500:
                    raise
                if attempt == DOWNLOAD_ATTEMPTS:
                    raise
                delay = RETRY_DELAY * 2**(attempt-1)
                logger.warning('Download interrupted; retaining partial data for retry %s/%s in %ss: %s: %s', attempt+1, DOWNLOAD_ATTEMPTS, delay, remote_url, error)
                time.sleep(delay)


def ensure_safe_name(filename: str) -> str:
    filename = filename.replace("\0", " ")
    if filename == ".":
        return " ."
    elif filename == "..":
        return ". ."
    else:
        return filename.replace("/", "\\").replace("\\", "_")


def release_generator(repo: str, base_url: str, perpage: int = 0, latest_only: bool = False):
    endpoint = f"{base_url}{repo}/releases"
    url = endpoint + ('/latest' if latest_only else (f'?per_page={perpage}' if perpage > 0 else ''))
    while True:
        try:
            with github_get(url) as response:
                response.raise_for_status()
                releases = response.json()
                links_header = response.headers.get('Link', '')
        except Exception:
            logger.exception('Failed to download release metadata for %s', repo)
            raise
        if latest_only:
            if not isinstance(releases, dict):
                raise ValueError('GitHub latest release metadata is not an object')
            yield releases
            return
        if not isinstance(releases, list):
            raise ValueError('GitHub releases metadata is not a list')
        yield from releases
        links = requests.utils.parse_header_links(links_header) if links_header else []
        next_link = next((link for link in links if link.get('rel') == 'next'), None)
        if not next_link:
            return
        url = next_link['url']


def main():
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default=BASE_URL)
    parser.add_argument("--working-dir", default=WORKING_DIR)
    parser.add_argument(
        "--workers", default=1, type=int, help="number of concurrent downloading jobs"
    )
    parser.add_argument(
        "--fast-skip",
        action="store_true",
        help="do not verify size and timestamp of existing files",
    )
    parser.add_argument("--config", default=CONFIG)
    args = parser.parse_args()

    if args.working_dir is None:
        raise Exception("Working Directory is None")

    working_dir = Path(args.working_dir)
    remote_filelist = []
    cleaning = False

    with open(args.config, "r") as f:
        REPOS = json.load(f)

    executor = concurrent.futures.ThreadPoolExecutor(max_workers=args.workers)
    futures = []

    def process_release(
        release: dict,
        release_dir: Path,
        tarball: bool,
        exclude_regexes: list[str],
    ) -> int:

        release_size = 0
        exclude_re = re.compile("|".join(exclude_regexes)) if exclude_regexes else None

        if tarball:
            url = release["tarball_url"]
            updated = datetime.strptime(
                release["published_at"], "%Y-%m-%dT%H:%M:%SZ"
            ).timestamp()
            dst_file = release_dir / "repo-snapshot.tar.gz"
            remote_filelist.append(dst_file.relative_to(working_dir))

            if dst_file.is_file():
                logger.info(f"skipping {dst_file.relative_to(working_dir)}")
            else:
                dst_file.parent.mkdir(parents=True, exist_ok=True)
                # tarball has no size information, use -1 to skip size check
                logger.info(f"queueing download of {url} to {dst_file.relative_to(working_dir)}")
                futures.append(
                    executor.submit(
                        download_file, url, dst_file, working_dir, updated, -1
                    )
                )

        for asset in release["assets"]:
            if exclude_re and exclude_re.search(asset["name"]):
                logger.info(f"excluding {asset['name']} by regex")
                continue

            url = asset["browser_download_url"]
            updated = datetime.strptime(
                asset["updated_at"], "%Y-%m-%dT%H:%M:%SZ"
            ).timestamp()
            dst_file = release_dir / ensure_safe_name(asset["name"])
            remote_filelist.append(dst_file.relative_to(working_dir))
            remote_size = asset["size"]
            release_size += remote_size

            if dst_file.is_file():
                if args.fast_skip:
                    logger.info(f"fast skipping {dst_file.relative_to(working_dir)}")
                    continue
                else:
                    stat = dst_file.stat()
                    local_filesize = stat.st_size
                    local_mtime = stat.st_mtime
                    if (
                        local_mtime > updated
                        or remote_size == local_filesize
                        and local_mtime == updated
                    ):
                        logger.info(f"skipping {dst_file.relative_to(working_dir)}")
                        continue
            else:
                dst_file.parent.mkdir(parents=True, exist_ok=True)

            logger.info(f"queueing download of {url} to {dst_file.relative_to(working_dir)}")
            futures.append(
                executor.submit(
                    download_file, url, dst_file, working_dir, updated, remote_size, asset.get("digest")
                )
            )

        return release_size

    def download_file(
        url: str, dst_file: Path, working_dir: Path, updated: float, remote_size: int, remote_digest: str | None = None
    ) -> bool:
        logger.info(f"downloading {url} to {dst_file.relative_to(working_dir)} ({remote_size} bytes)")
        try:
            do_download(url, dst_file, updated, remote_size, remote_digest)
            return True
        except Exception as e:
            logger.error(f"Failed to download {url}: {e}")
            return False

    def link_latest(name: str, repo_dir: Path) -> None:
        try:
            os.unlink(repo_dir / "LatestRelease")
        except OSError:
            pass
        try:
            os.symlink(name, repo_dir / "LatestRelease")
        except OSError:
            pass

    total_size = 0
    meta_failed = False

    for cfg in REPOS:
        if isinstance(cfg, str):
            cfg = {"repo": cfg}
        repo = cfg["repo"]
        versions = cfg.get("versions", 1)  # keep # of latest releases (mixed)
        release_versions = cfg.get("release_versions")  # # of stable releases only
        pre_release_versions = cfg.get("pre_release_versions")  # # of pre-releases only
        flat = cfg.get("flat", False)  # build a folder for each release
        tarball = cfg.get("tarball", False)  # download source tarball
        prerelease = cfg.get("pre_release", False)  # include pre-releases
        perpage = cfg.get("per_page", 0)  # number of releases per page
        exclude_regexes = cfg.get("exclude", [])  # list of file name regexes to exclude

        # Determine mode: new fields take priority over versions
        use_separate_limits = release_versions is not None or pre_release_versions is not None
        if use_separate_limits:
            max_release = release_versions if release_versions is not None else 1
            max_prerelease = pre_release_versions if pre_release_versions is not None else 0
            # When using separate limits, always include pre-releases
            prerelease = True
        else:
            max_release = versions
            max_prerelease = 0

        repo_dir = working_dir / Path(repo)
        logger.info(f"syncing {repo} to {repo_dir}")

        n_downloaded = 0
        n_release = 0
        n_prerelease = 0
        try:
            for release in release_generator(
                repo, args.base_url, perpage,
                latest_only=(versions == 1 and not prerelease and not use_separate_limits),
            ):
                if release["draft"]:
                    continue
                is_prerelease = release["prerelease"]
                if is_prerelease and not prerelease:
                    continue

                # Check version limits
                if use_separate_limits:
                    # Separate limits mode: skip if respective cap is 0 or reached
                    # -1 means unlimited
                    if is_prerelease:
                        if max_prerelease == 0:
                            continue
                        if max_prerelease > 0 and n_prerelease >= max_prerelease:
                            continue
                    else:
                        if max_release == 0:
                            continue
                        if max_release > 0 and n_release >= max_release:
                            continue
                else:
                    # Legacy mode: single counter against versions
                    if versions > 0 and n_downloaded >= versions:
                        continue

                name = ensure_safe_name(release["name"] or release["tag_name"])
                if len(name) == 0:
                    logger.error("Unnamed release")
                    continue
                total_size += process_release(
                    release,
                    (repo_dir if flat else repo_dir / name),
                    tarball,
                    exclude_regexes,
                )
                if n_downloaded == 0 and not flat:
                    # create a symbolic link to the latest release folder
                    link_latest(name, repo_dir)
                n_downloaded += 1
                if is_prerelease:
                    n_prerelease += 1
                else:
                    n_release += 1

                # Check if done
                if use_separate_limits:
                    release_done = max_release == 0 or (max_release > 0 and n_release >= max_release)
                    prerelease_done = max_prerelease == 0 or (max_prerelease > 0 and n_prerelease >= max_prerelease)
                    if release_done and prerelease_done:
                        break
                else:
                    if versions > 0 and n_downloaded >= versions:
                        break
            if n_downloaded == 0:
                logger.error(f"No release version found for {repo}")
                meta_failed = True
                continue
        except Exception:
            meta_failed = True
            logger.exception(f"Failed to process releases for {repo}")
    if not meta_failed:
        cleaning = True

    results, _ = concurrent.futures.wait(futures)
    executor.shutdown()
    all_success = (not results) or all(r.result() for r in results)
    if meta_failed:
        all_success = False

    # Only delete extras after every repo's metadata was fetched and every
    # queued download succeeded. An empty remote list would wipe the mirror.
    if cleaning and all_success and remote_filelist:
        local_filelist: list[Path] = []
        for local_file in working_dir.glob("**/*"):
            if local_file.is_file():
                local_filelist.append(local_file.relative_to(working_dir))

        for old_file in set(local_filelist) - set(remote_filelist):
            logger.info(f"deleting {old_file}")
            old_file = working_dir / old_file
            old_file.unlink()

        for local_dir in working_dir.glob("*/*/*"):
            if local_dir.is_dir():
                try:
                    local_dir.rmdir()
                    logger.info(f"Removing empty directory {local_dir}")
                except Exception:
                    pass
    elif meta_failed:
        logger.error("Skipping cleanup because some GitHub metadata requests failed")

    logger.info(f"Total size is {sizeof_fmt(total_size, suffix='')}")
    print(f"SYNORA_SIZE={total_size}", flush=True)
    if not all_success:
        logger.error("Some files failed to download")
        print("SYNORA_STATUS=failed", flush=True)
        raise SystemExit(1)
    print("SYNORA_STATUS=success", flush=True)

if __name__ == "__main__":
    main()


# vim: ts=4 sw=4 sts=4 expandtab
