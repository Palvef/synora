#!/usr/bin/env python3
"""yuki2synora — migrate Yuki repo YAML configs to Synora job TOML files.

Reads Yuki per-repository YAML files (see ustclug/yuki README: name, cron,
storageDir, image, envs, logRotCycle) and writes one TOML job per repo, plus
a main synora.toml with include + daemon settings.

Mapping:
  cron: "0 * * * *"    -> schedule = "cron", cron = <same>   (Synora's cron is
                          minute-granular and no-drift; Yuki crons carry over)
  storageDir           -> storage
  image                -> provider = "docker", image, env from envs,
                          storage mounted at /data by Synora (Yuki convention)
  logRotCycle          -> LOG_ROTATE_CYCLE inside the image; the Synora job
                          log directory is mounted at /log
  retry                -> RETRY inside the image and Synora retry = 0, avoiding
                          two nested retry loops
  network              -> docker_network (Yuki's empty default becomes host)

Usage:
  python3 yuki2synora.py /etc/yuki/repo-configs/ -o config/ [--db data/synora.db]

Only the Python standard library (YAML subset used by Yuki: plain
key: value mappings with string scalars — no anchors/aliases). Offline-safe.
"""

import argparse
import os
from pathlib import Path

TOML_ESCAPE = str.maketrans({"\\": "\\\\", '"': '\\"'})


def toml_str(s):
    return '"' + str(s).translate(TOML_ESCAPE) + '"'


def strip_yaml_comment(line):
    """Remove a YAML comment without truncating `#` inside a quoted value."""
    quote = None
    escaped = False
    for index, char in enumerate(line):
        if escaped:
            escaped = False
            continue
        if char == "\\" and quote == '"':
            escaped = True
            continue
        if char in ("'", '"'):
            if quote == char:
                quote = None
            elif quote is None:
                quote = char
            continue
        if char == "#" and quote is None and (index == 0 or line[index - 1].isspace()):
            return line[:index]
    return line


def parse_yaml_simple(text):
    """Parse the flat `key: value` YAML subset Yuki uses.

    Values: quoted strings, unquoted scalars, and inline lists [a, b].
    Nested maps support Yuki's `envs` and `volumes` blocks.
    """
    out = {}
    nested_key = None
    nested = None
    for line in text.splitlines():
        line = strip_yaml_comment(line).rstrip()
        if not line.strip():
            continue
        stripped = line.strip()
        if line.startswith(" ") or line.startswith("\t"):
            # indented line → nested map entry (Yuki's `envs:` block)
            if nested is not None and ":" in stripped:
                k, _, v = stripped.partition(":")
                nested[k.strip()] = v.strip().strip('"\'')
            continue
        if nested is not None:
            out[nested_key] = nested
            nested_key = None
            nested = None
        if ":" not in line:
            continue
        key, _, value = line.partition(":")
        key = key.strip()
        value = value.strip()
        if not value:
            nested_key = key
            nested = {}
            continue
        if value.startswith('"') and value.endswith('"'):
            value = value[1:-1]
        elif value.startswith("'") and value.endswith("'"):
            value = value[1:-1]
        elif value.startswith("[") and value.endswith("]"):
            value = [v.strip().strip('"\'') for v in value[1:-1].split(",") if v.strip()]
        out[key] = value
    if nested is not None:
        out[nested_key] = nested
    return out


def render_job(
    repo,
    log_dir="/var/log/synora",
    sync_timeout=None,
    default_owner=None,
    default_bind_ip=None,
):
    name = str(repo.get("name", ""))
    if not name:
        raise SystemExit("repo config without `name`")
    lines = ["[[jobs]]", f"name = {toml_str(name)}", "enabled = true", ""]

    cron = str(repo.get("cron") or "").strip()
    if cron.startswith("@every "):
        lines.append('schedule = "interval"')
        lines.append(f"every = {toml_str(cron.removeprefix('@every ').strip())}")
    elif cron:
        lines.append('schedule = "cron"')
        lines.append(f"cron = {toml_str(cron)}")
    else:
        lines.append('schedule = "manual"  # no cron in Yuki config')

    lines.append("")

    image = repo.get("image")
    if image:
        lines.append('provider = "docker"')
        lines.append(f"image = {toml_str(str(image))}")
        env_map = dict(repo.get("envs") or {}) if isinstance(repo.get("envs"), dict) else {}
        # Yuki injects these fields before starting every image. Preserve
        # them because ustcmirror images consume RETRY/OWNER internally.
        env_map.setdefault("REPO", name)
        env_map.setdefault("RETRY", str(repo.get("retry", 0)))
        env_map.setdefault("LOG_ROTATE_CYCLE", str(repo.get("logRotCycle", 0)))
        owner = repo.get("user") or default_owner
        bind_ip = repo.get("bindIP") or default_bind_ip
        if owner:
            env_map.setdefault("OWNER", str(owner))
        if bind_ip:
            env_map.setdefault("BIND_ADDRESS", str(bind_ip))
        envs = [f"{key}={value}" for key, value in env_map.items()]
        if envs:
            lines.append(f"env = {toml_list(envs)}")
        volumes = repo.get("volumes")
        volume_list = [f"{log_dir.rstrip('/')}/{name}:/log"]
        if isinstance(volumes, dict):
            volume_list.extend(f"{source}:{target}" for source, target in volumes.items())
        lines.append(f"volumes = {toml_list(volume_list)}")
        # Yuki treats an empty network as host networking, not Docker's
        # bridge default.
        lines.append(f"docker_network = {toml_str(repo.get('network') or 'host')}")
    else:
        # Yuki always syncs via an image; without one, fall back to script.
        lines.append('provider = "script"')
        lines.append('command = "true"  # no image in Yuki config — fill me in"')

    storage = repo.get("storageDir")
    if storage:
        lines.append(f"storage = {toml_str(str(storage))}")
    else:
        raise SystemExit(f"repo {name}: missing storageDir")
    # Yuki's RETRY is consumed inside the image. A second orchestration retry
    # loop would multiply attempts after migration.
    lines.append("retry = 0")
    if sync_timeout:
        lines.append(f"timeout = {toml_str(sync_timeout)}")
    return "\n".join(lines) + "\n"


def toml_list(items):
    return "[" + ", ".join(toml_str(i) for i in items) + "]"


def main():
    ap = argparse.ArgumentParser(description="migrate Yuki repo YAMLs to Synora config")
    ap.add_argument("dir", help="Yuki repo-config directory (or a single .yaml file)")
    ap.add_argument("-o", "--out", default="config", help="output config dir (default: config)")
    ap.add_argument("--log-dir", default="/var/log/synora", help="synora log dir")
    ap.add_argument("--db", default="data/synora.db", help="synora sqlite db path")
    ap.add_argument(
        "--sync-timeout",
        help="Yuki daemon sync_timeout to apply to every generated job (for example 48h)",
    )
    ap.add_argument(
        "--owner",
        default=f"{os.getuid()}:{os.getgid()}",
        help="Yuki daemon owner injected as OWNER (default: current uid:gid)",
    )
    ap.add_argument("--bind-ip", help="Yuki daemon bind_ip fallback")
    args = ap.parse_args()

    src = Path(args.dir)
    files = [src] if src.is_file() else sorted(src.glob("*.yaml")) + sorted(src.glob("*.yml"))
    if not files:
        raise SystemExit(f"no .yaml files under {args.dir}")

    out = Path(args.out)
    jobs_dir = out / "jobs"
    jobs_dir.mkdir(parents=True, exist_ok=True)

    count = 0
    for f in files:
        repo = parse_yaml_simple(f.read_text(encoding="utf-8"))
        if "name" not in repo:
            print(f"skip {f}: no `name` key")
            continue
        target = jobs_dir / f"{repo['name']}.toml"
        target.write_text(
            render_job(
                repo,
                log_dir=args.log_dir,
                sync_timeout=args.sync_timeout,
                default_owner=args.owner,
                default_bind_ip=args.bind_ip,
            ),
            encoding="utf-8",
        )
        print(f"  {repo['name']}: {target}")
        count += 1

    main_toml = f'''# Generated by yuki2synora from {args.dir}
include = ["jobs/*.toml"]

[daemon]
log_dir = {toml_str(args.log_dir)}

[daemon.db]
kind = "sqlite"
path = {toml_str(args.db)}

[api]
listen = "127.0.0.1:8100"
'''
    (out / "synora.toml").write_text(main_toml, encoding="utf-8")
    print(f"wrote {out / 'synora.toml'} ({count} job(s))")
    print(f"next: `synora check -c {out}/synora.toml`")


if __name__ == "__main__":
    main()
