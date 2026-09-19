#!/usr/bin/env python3
import argparse
import fcntl
import bz2
import gzip
import lzma
import logging
import os
import re
import shutil
import sys
import sqlite3
import subprocess as sp
import tempfile
import time
import traceback
import xml.etree.ElementTree as ET
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import Dict, List

import requests

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

REPO_SIZE_FILE = os.getenv("REPO_SIZE_FILE", "")
DOWNLOAD_TIMEOUT = int(os.getenv("DOWNLOAD_TIMEOUT", "1800"))
REPO_STAT = {}


from repo_discovery import directories, rpm_versions

def repository_matrix(template, os_values, components, arches):
    values = {'os_ver': os_values, 'comp': components, 'arch': arches}
    def walk(url, remaining, bindings):
        if not remaining:
            yield bindings, url.rstrip('/')
            return
        part, *tail = remaining
        keys = re.findall(r'@\{(os_ver|comp|arch)\}', part)
        if not keys:
            yield from walk(url+'/'+part, tail, bindings)
            return
        if len(keys) != 1: raise ValueError('Only one discovery field per URL segment is supported')
        key=keys[0]
        candidates=values[key]
        if key == 'os_ver' and len(candidates)==1 and candidates[0] in ('@rhel-current','@fedora-current'):
            candidates=rpm_versions(candidates[0])
        elif any(v.startswith('@') for v in candidates):
            rx=re.compile(re.escape(part).replace(re.escape('@{'+key+'}'), '(.+)')+'$')
            candidates=[m.group(1) for name in directories(url) if (m:=rx.fullmatch(name))]
        for value in candidates:
            yield from walk(url+'/'+part.replace('@{'+key+'}',value),tail,{**bindings,key:value})
    origin, path = template.split('://',1)
    host, _, path = path.partition('/')
    yield from walk(origin+'://'+host,path.rstrip('/').split('/'),{})

def calc_repo_size(path: Path):
    # repomd.xml is authoritative; stale primary files must not be selected.
    root = ET.parse(path / 'repodata/repomd.xml').getroot()
    ns = {'r': 'http://linux.duke.edu/metadata/repo'}
    primary = root.find("r:data[@type='primary']/r:location", ns)
    if primary is None: raise RuntimeError(f'No primary metadata in {path}')
    db = (path / primary.attrib['href']).resolve()
    if not db.is_relative_to(path.resolve()): raise RuntimeError('Unsafe primary metadata path')
    suffixes = db.suffixes
    with tempfile.NamedTemporaryFile() as tmp:
        if suffixes[-1] == '.zst':
            sp.run(['zstd', '-dc', str(db)], stdout=tmp, check=True)
            suffixes = suffixes[:-1]
        else:
            dec = {'.gz': gzip.decompress, '.bz2': bz2.decompress, '.xz': lzma.decompress}.get(suffixes[-1])
            tmp.write(dec(db.read_bytes()) if dec else db.read_bytes())
            if dec: suffixes = suffixes[:-1]
        tmp.flush()

        if suffixes[-1] == ".sqlite":
            conn = sqlite3.connect(tmp.name)
            c = conn.cursor()
            c.execute("select sum(size_package),count(1) from packages")
            size, cnt = c.fetchone()
            conn.close()
        elif suffixes[-1] == ".xml":
            try:
                tree = ET.parse(tmp.name)
                root = tree.getroot()
                assert root.tag.endswith("metadata")
                cnt, size = 0, 0
                for location in root.findall(
                    "./{http://linux.duke.edu/metadata/common}package/{http://linux.duke.edu/metadata/common}size"
                ):
                    size += int(location.attrib["package"])
                    cnt += 1
            except:
                traceback.print_exc()
                raise
        else:
            raise RuntimeError(f"Unknown suffix {suffixes}")

        logger.info(f"Repository {path}:")
        logger.info(f"  {cnt} packages, {size} bytes in total")

        global REPO_STAT
        REPO_STAT[str(path)] = (size, cnt) if cnt > 0 else (0, 0)  # size can be None


def check_and_download(url: str, dst_file: Path) -> int:
    try:
        start = time.time()
        with requests.get(url, stream=True, timeout=(30, 60)) as r:
            r.raise_for_status()
            if "last-modified" in r.headers:
                remote_ts = parsedate_to_datetime(
                    r.headers["last-modified"]
                ).timestamp()
            else:
                remote_ts = None

            with dst_file.open("wb") as f:
                for chunk in r.iter_content(chunk_size=1024**2):
                    if time.time() - start > DOWNLOAD_TIMEOUT:
                        raise TimeoutError("Download timeout")
                    if not chunk:
                        continue  # filter out keep-alive new chunks

                    f.write(chunk)
            if remote_ts is not None:
                os.utime(dst_file, (remote_ts, remote_ts))
        return 0
    except BaseException as e:
        logger.error(f"Error occurred: {e}")
        if dst_file.is_file():
            dst_file.unlink()
    return 1


def download_repodata(url: str, path: Path) -> int:
    path = path / "repodata"
    path.mkdir(exist_ok=True)
    oldfiles = set(path.glob("*.*"))
    newfiles = set()
    if check_and_download(url + "/repodata/repomd.xml", path / ".repomd.xml") != 0:
        logger.error(f"Failed to download the repomd.xml of {url}")
        return 1
    try:
        tree = ET.parse(path / ".repomd.xml")
        root = tree.getroot()
        assert root.tag.endswith("repomd")
        for location in root.findall(
            "./{http://linux.duke.edu/metadata/repo}data/{http://linux.duke.edu/metadata/repo}location"
        ):
            href = location.attrib["href"]
            assert len(href) > 9 and href[:9] == "repodata/"
            fn = path / href[9:]
            newfiles.add(fn)
            if check_and_download(url + "/" + href, fn) != 0:
                logger.error(f"Failed to download the {href}")
                return 1
    except BaseException as e:
        traceback.print_exc()
        return 1

    (path / ".repomd.xml").rename(path / "repomd.xml")  # update the repomd.xml
    newfiles.add(path / "repomd.xml")
    for i in oldfiles - newfiles:
        logger.info(f"Deleting old files: {i}")
        i.unlink()


def check_args(prop: str, lst: List[str]):
    for s in lst:
        if len(s) == 0 or " " in s:
            raise ValueError(f"Invalid item in {prop}: {repr(s)}")


def substitute_vars(s: str, vardict: Dict[str, str]) -> str:
    for key, val in vardict.items():
        tpl = "@{" + key + "}"
        s = s.replace(tpl, val)
    return s


from repo_selection import Selection

def main():

    parser = argparse.ArgumentParser()
    parser.add_argument("base_url", type=str, help="base URL")
    parser.add_argument("os_version", type=str, help="e.g. 7-8,9")
    parser.add_argument(
        "component", type=str, help="e.g. mysql56-community,mysql57-community"
    )
    parser.add_argument("arch", type=str, help="e.g. x86_64,aarch64")
    parser.add_argument("repo_name", type=str, help="e.g. @{comp}-el@{os_ver}")
    parser.add_argument("working_dir", type=Path, help="working directory")
    parser.add_argument(
        "--download-repodata",
        action="store_true",
        help="download repodata files instead of generating them",
    )
    parser.add_argument(
        "--pass-arch-to-reposync",
        action="store_true",
        help="""pass --arch to reposync to further filter packages by 'arch' field in metadata (NOT recommended, prone to missing packages in some repositories, e.g. mysql)""",
    )
    parser.add_argument("--dry-run", action="store_true", help="probe discovered repositories without syncing")
    parser.add_argument("--legacy-x86-repo-names", action="store_true", help="omit the x86_64 suffix for compatibility with existing MongoDB mirrors")
    args = parser.parse_args()

    raw_os_list = args.os_version.split(",")
    os_list = []
    for os_version in raw_os_list:
        if re.fullmatch(r"[0-9]+-[0-9]+", os_version):
            dash = os_version.index("-")
            os_list = os_list + [
                str(i)
                for i in range(int(os_version[:dash]), 1 + int(os_version[dash + 1 :]))
            ]
        else:
            os_list.append(os_version)
    check_args("os_version", os_list)
    component_list = args.component.split(",")
    check_args("component", component_list)
    arch_list = args.arch.split(",")
    check_args("arch", arch_list)

    logger.info(f"Configuration: {os_list=}, {component_list=}, {arch_list=}")

    selection = Selection()
    failed = []
    missing_repositories = set()
    if not args.dry_run: args.working_dir.mkdir(parents=True, exist_ok=True)
    # Hold a repository-root lock before touching interrupted createrepo state.
    sync_lock = None
    if not args.dry_run:
        sync_lock = (args.working_dir / '.yum-sync.lock').open('a')
        fcntl.flock(sync_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    cache_dir = tempfile.mkdtemp()

    def combination_os_comp(arch):
        matrix = list(repository_matrix(args.base_url, os_list, component_list, [arch]))
        found = set()
        for bindings, url in matrix:
            vardict = {'os_ver': os_list[0], 'comp': component_list[0], 'arch': arch, **bindings}
            name = substitute_vars(args.repo_name, vardict)
            if args.legacy_x86_repo_names and vardict["arch"] == "x86_64":
                name = name.removesuffix("-x86_64")
            enabled = selection.allows(vardict['os_ver'], vardict['arch'], vardict['comp'])
            logger.info('Selection: version=%s architecture=%s component=%s sync=%s', vardict['os_ver'], vardict['arch'], vardict['comp'], 'yes' if enabled else 'no')
            if not enabled: continue
            if (name,url) in found: continue
            found.add((name,url))
            probe_url = url+'/repodata/repomd.xml'
            r = requests.get(probe_url, timeout=(30,60))
            if r.status_code in (404,410):
                logger.info('Unavailable repository: %s', probe_url)
                missing_repositories.add(name)
                continue
            r.raise_for_status()
            if not ET.fromstring(r.content).tag.endswith('repomd'): raise RuntimeError(f'Invalid repomd: {probe_url}')
            yield name,url

    if args.dry_run:
        count=0
        for arch in arch_list:
            for name,url in combination_os_comp(arch):
                print(name, url, flush=True);count+=1
        if not count and not selection.active: raise RuntimeError('No available RPM repositories discovered')
        return
    # Isolate each repository: one broken historical branch must not stop all others.
    import mongodb_rpm
    from package_policy import NAME_PATTERNS, prune_excluded
    repaired = []
    for arch in arch_list:
        found = False
        for name, repo_url in combination_os_comp(arch):
            found = True
            path = (args.working_dir / name).absolute()
            path.mkdir(parents=True, exist_ok=True)
            try:
                recovery = mongodb_rpm.needs_repair(repo_url)
                if recovery:
                    mongodb_rpm.recover(repo_url, path)
                    repaired.append(repo_url)
                else:
                    with tempfile.NamedTemporaryFile('w', suffix='.conf') as conf:
                        conf.write(f"[main]\nreposdir=/dev/null\nkeepcache=0\nskip_if_unavailable=0\n[{name}]\nname={name}\nbaseurl={repo_url}\nrepo_gpgcheck=0\ngpgcheck=0\nenabled=1\nskip_if_unavailable=0\nexclude={' '.join(NAME_PATTERNS)}\n")
                        conf.flush()
                        command = ['dnf', '--disableplugin=local,system_upgrade', 'reposync', '-c', conf.name, '--delete', '-p', str(args.working_dir.absolute())]
                        if args.pass_arch_to_reposync:
                            command += ['--arch', arch]
                        logger.info('Syncing repository %s from %s', name, repo_url)
                        sp.run(command, check=True)
                if args.download_repodata and not recovery:
                    if download_repodata(repo_url, path):
                        raise RuntimeError('Failed to download repository metadata')
                logger.info('Removed %d excluded debug/test packages', prune_excluded(path))
                # Regenerate indexes for the filtered payload inventory.
                shutil.rmtree(path / '.repodata', True)
                sp.run(['createrepo_c', '--update', '-c', cache_dir, '-o', str(path), str(path)], check=True)
                calc_repo_size(path)
            except Exception as error:
                logger.error('Repository %s failed: %s; retaining metadata and continuing other repositories', name, error)
                failed.append((str(path), arch))
        if not found and not selection.active:
            failed.append(('', arch))
    if not failed and not selection.active:
        for name in missing_repositories:
            obsolete = args.working_dir / name
            if obsolete.exists():
                if obsolete.is_symlink() or not obsolete.resolve().is_relative_to(args.working_dir.resolve()):
                    raise RuntimeError('RPM cleanup path escapes repository')
                logger.info('Removing unavailable upstream repository %s', name)
                shutil.rmtree(obsolete)
    shutil.rmtree(cache_dir)
    if repaired and not failed:
        print('SYNORA_STATUS=success_with_warnings')
        logger.warning('Rebuilt missing upstream RPM indexes: %s', repaired)

    if len(failed) > 0:
        logger.error(f"Failed YUM repos: {failed}")
        sys.exit(1)
    if len(REPO_SIZE_FILE) > 0:
        with open(REPO_SIZE_FILE, "a") as fd:
            total_size = sum([r[0] for r in REPO_STAT.values()])
            fd.write(f"+{total_size}")


if __name__ == "__main__":
    main()
