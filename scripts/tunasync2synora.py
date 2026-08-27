#!/usr/bin/env python3
"""Migrate tunasync worker TOML into Synora job TOML files.

The converter reads the same TOML shape as tunasync, including files selected
by ``[include].include_mirrors``. It uses only the Python 3.11+ standard
library.
"""

import argparse
import glob
import shlex
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError as exc:  # pragma: no cover - Python < 3.11
    raise SystemExit("tunasync2synora requires Python 3.11 or newer") from exc


TOML_ESCAPE = str.maketrans({"\\": "\\\\", '"': '\\"'})


def toml_str(value):
    return '"' + str(value).translate(TOML_ESCAPE) + '"'


def toml_list(items):
    return "[" + ", ".join(toml_str(item) for item in items) + "]"


def parse_list(value):
    """Return a lenient string list for already-decoded TOML values."""
    if value is None:
        return []
    if isinstance(value, (list, tuple)):
        return [str(item) for item in value]
    if isinstance(value, dict):
        return [f"{key}={item}" for key, item in value.items()]
    value = str(value).strip()
    return [value] if value else []


def env_list(value):
    if isinstance(value, dict):
        return [f"{key}={item}" for key, item in value.items()]
    return parse_list(value)


def int_codes(*values):
    result = []
    for value in values:
        for item in parse_list(value):
            try:
                code = int(item)
            except (TypeError, ValueError) as exc:
                raise SystemExit(f"invalid tunasync success exit code {item!r}") from exc
            if code not in result:
                result.append(code)
    return result


def effective_hooks(mirror, global_cfg, key):
    base = parse_list(mirror[key]) if key in mirror else parse_list(global_cfg.get(key))
    base.extend(parse_list(mirror.get(f"{key}_extra")))
    return base


def docker_volumes(mirror, docker_cfg):
    volumes = parse_list(docker_cfg.get("volumes"))
    volumes.extend(parse_list(mirror.get("docker_volumes")))
    exclude_file = mirror.get("exclude_file")
    if exclude_file:
        volumes.append(f"{exclude_file}:{exclude_file}:ro")
    return volumes


def storage_lines(mirror):
    # A per-mirror mirror_dir is the complete destination in tunasync and
    # overrides global mirror_dir/mirror_subdir/name composition.
    if mirror.get("mirror_dir"):
        return [f"storage = {toml_str(mirror['mirror_dir'])}"]
    relative = str(mirror["name"])
    if mirror.get("mirror_subdir"):
        relative = f"{mirror['mirror_subdir']}/{relative}"
    return [f"storage = {toml_str(relative)}", 'storage_name = "mirror"']


def render_job(mirror, global_cfg, docker_cfg):
    name = str(mirror.get("name", "")).strip()
    if not name:
        raise SystemExit("tunasync mirror without `name`")
    lines = ["[[jobs]]", f"name = {toml_str(name)}", "enabled = true", ""]

    interval = mirror.get("interval") or global_cfg.get("interval")
    if interval:
        lines.extend(['schedule = "interval"', f"every = {toml_str(f'{int(interval)}m')}"])
    else:
        lines.append('schedule = "manual"  # no interval configured in tunasync')
    lines.append("")

    provider = str(mirror.get("provider", "rsync"))
    environment = env_list(mirror.get("env"))

    if provider in ("rsync", "two-stage-rsync"):
        lines.append(f"provider = {toml_str(provider)}")
        lines.append(f"upstream = {toml_str(mirror.get('upstream', ''))}")
        if provider == "two-stage-rsync":
            lines.append(f"stage1_profile = {toml_str(mirror.get('stage1_profile') or 'debian')}")

        options = parse_list(global_cfg.get("rsync_options"))
        options.extend(parse_list(mirror.get("rsync_options")))
        if mirror.get("rsync_no_timeout"):
            options.append("--timeout=0")
        elif mirror.get("rsync_timeout"):
            options.append(f"--timeout={int(mirror['rsync_timeout'])}")
        if mirror.get("exclude_file"):
            options.append(f"--exclude-from={mirror['exclude_file']}")
        options = [
            option.replace("/etc/tunasync/syncpassword/", "/etc/synora/syncpassword/")
            .replace("/etc/tunasync/excludes/", "/etc/synora/excludes/")
            for option in options
        ]
        if options:
            lines.append(f"options = {toml_list(options)}")
        if mirror.get("use_ipv6"):
            lines.append('family = "ipv6"')
        elif mirror.get("use_ipv4"):
            lines.append('family = "ipv4"')

        # tunasync merges all four lists. Emit [] explicitly when empty so
        # Synora's permissive 23/24 default does not alter migrated behavior.
        codes = int_codes(
            global_cfg.get("dangerous_global_success_exit_codes"),
            mirror.get("success_exit_codes"),
            global_cfg.get("dangerous_global_rsync_success_exit_codes"),
            mirror.get("rsync_success_exit_codes"),
        )
        lines.append("success_exit_codes = [" + ", ".join(map(str, codes)) + "]")
    elif provider == "command":
        command = str(mirror.get("command", ""))
        migrated_command = command.replace(
            "/home/tunasync-scripts/", "/usr/lib/synora/scripts/"
        )
        is_git = command.rstrip().endswith("/git.sh") or command.strip() == "git.sh"
        if is_git and not mirror.get("docker_image") and not environment:
            lines.extend(
                ['provider = "git"', f"upstream = {toml_str(mirror.get('upstream', ''))}"]
            )
        elif mirror.get("docker_image"):
            lines.extend(
                [
                    'provider = "docker"',
                    f"image = {toml_str(mirror['docker_image'])}",
                    f"upstream = {toml_str(mirror.get('upstream', ''))}",
                ]
            )
            # Keep the original in-container path: tunasync's global volume
            # commonly mounts /home/tunasync-scripts into a custom image.
            args = shlex.split(command) if command.strip() else []
            if args:
                lines.append(f"docker_command = {toml_list(args)}")
            if environment:
                lines.append(f"env = {toml_list(environment)}")
            docker_options = parse_list(docker_cfg.get("options"))
            docker_options.extend(parse_list(mirror.get("docker_options")))
            if docker_options:
                lines.append(f"docker_options = {toml_list(docker_options)}")
            volumes = docker_volumes(mirror, docker_cfg)
            if volumes:
                lines.append(f"volumes = {toml_list(volumes)}")
        else:
            lines.extend(
                [
                    'provider = "script"',
                    f"command = {toml_str(migrated_command)}",
                    f"upstream = {toml_str(mirror.get('upstream', ''))}",
                ]
            )
            if environment:
                lines.append(f"env = {toml_list(environment)}")
        if mirror.get("fail_on_match"):
            lines.append(f"fail_on_match = {toml_str(mirror['fail_on_match'])}")
    else:
        raise SystemExit(f"unsupported provider {provider!r} for mirror {name}")

    lines.extend(storage_lines(mirror))
    # tunasync's value is the total number of attempts (its loop is
    # `retry < provider.Retry()`); Synora's value is retries after the first
    # attempt. tunasync also defaults zero/missing global retry to 2.
    tunasync_attempts = int(mirror.get("retry") or global_cfg.get("retry") or 2)
    retry = max(tunasync_attempts - 1, 0)
    timeout = mirror.get("timeout") or global_cfg.get("timeout")
    lines.append(f"retry = {retry}")
    if timeout:
        lines.append(f"timeout = {int(timeout)}")
    if mirror.get("memory_limit"):
        lines.append(f"memory_limit = {toml_str(mirror['memory_limit'])}")

    on_success = effective_hooks(mirror, global_cfg, "exec_on_success")
    on_failure = effective_hooks(mirror, global_cfg, "exec_on_failure")
    if on_success or on_failure:
        lines.extend(["", "[jobs.hooks]"])
        if on_success:
            lines.append(f"on_success = {toml_list(on_success)}")
        if on_failure:
            lines.append(f"on_failure = {toml_list(on_failure)}")
    return "\n".join(lines) + "\n"


def load_tunasync(path):
    with path.open("rb") as source:
        root = tomllib.load(source)
    mirrors = list(root.get("mirrors", []))
    include_value = root.get("include", {}).get("include_mirrors")
    for pattern in parse_list(include_value):
        candidate = Path(pattern)
        if not candidate.is_absolute():
            candidate = path.parent / candidate
        for included in sorted(glob.glob(str(candidate))):
            with open(included, "rb") as source:
                child = tomllib.load(source)
            mirrors.extend(child.get("mirrors", []))
    return root, mirrors


def main():
    parser = argparse.ArgumentParser(description="migrate tunasync workers.conf to Synora")
    parser.add_argument("conf", help="tunasync workers.conf path")
    parser.add_argument("-o", "--out", default="config", help="output config directory")
    parser.add_argument("--log-dir", default="/var/log/synora")
    parser.add_argument("--db", default="data/synora.db")
    args = parser.parse_args()

    source = Path(args.conf)
    root, mirrors = load_tunasync(source)
    if "global" not in root:
        raise SystemExit(f"{source}: missing [global] section")
    global_cfg = root["global"]
    docker_cfg = root.get("docker", {})
    storage_dir = str(global_cfg.get("mirror_dir", "/srv/mirror"))

    out = Path(args.out)
    jobs_dir = out / "jobs"
    jobs_dir.mkdir(parents=True, exist_ok=True)
    for mirror in mirrors:
        target = jobs_dir / f"{mirror['name']}.toml"
        target.write_text(render_job(mirror, global_cfg, docker_cfg), encoding="utf-8")
        print(f"  {mirror['name']}: {target}")

    zfs_cfg = root.get("zfs", {})
    if zfs_cfg.get("enable"):
        storage = f'''kind = "zfs"
pool = {toml_str(zfs_cfg.get("zpool") or "data")}
dataset = "mirror"
mountpoint = {toml_str(storage_dir)}
auto_create = false'''
    else:
        storage = f'''kind = "dir"
mountpoint = {toml_str(storage_dir)}
auto_create = true'''
    concurrency = int(global_cfg.get("concurrent") or 8)
    main_toml = f'''# Generated by tunasync2synora from {source}
include = ["jobs/*.toml"]

[daemon]
max_concurrency = {concurrency}
log_dir = {toml_str(args.log_dir)}

[daemon.db]
kind = "sqlite"
path = {toml_str(args.db)}

[api]
listen = "127.0.0.1:8100"

[storage.mirror]
{storage}
'''
    (out / "synora.toml").write_text(main_toml, encoding="utf-8")
    print(f"wrote {out / 'synora.toml'} ({len(mirrors)} job(s))")
    print(f"next: `synora check -c {out}/synora.toml`")


if __name__ == "__main__":
    main()
