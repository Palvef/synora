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
  logRotCycle          -> dropped (Synora rotates current.log → YYYY-MM-DD.log)

Usage:
  python3 yuki2synora.py /etc/yuki/repo-configs/ -o config/ [--db data/synora.db]

Only the Python standard library (YAML subset used by Yuki: plain
key: value mappings with string scalars — no anchors/aliases). Offline-safe.
"""

import argparse
import re
from pathlib import Path

TOML_ESCAPE = str.maketrans({"\\": "\\\\", '"': '\\"'})


def toml_str(s):
    return '"' + str(s).translate(TOML_ESCAPE) + '"'


def parse_yaml_simple(text):
    """Parse the flat `key: value` YAML subset Yuki uses.

    Values: quoted strings, unquoted scalars, and inline lists [a, b].
    Nested maps are not used by Yuki repo configs.
    """
    out = {}
    nested_key = None
    nested = None
    for line in text.splitlines():
        line = line.split("#", 1)[0].rstrip()
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


def render_job(repo):
    name = str(repo.get("name", ""))
    if not name:
        raise SystemExit("repo config without `name`")
    lines = ["[[jobs]]", f"name = {toml_str(name)}", "enabled = true", ""]

    cron = repo.get("cron")
    if cron:
        lines.append('schedule = "cron"')
        lines.append(f"cron = {toml_str(str(cron))}")
    else:
        lines.append('schedule = "manual"  # no cron in Yuki config')

    lines.append("")

    image = repo.get("image")
    if image:
        lines.append('provider = "docker"')
        lines.append(f"image = {toml_str(str(image))}")
        envs = repo.get("envs")
        if isinstance(envs, dict):
            envs = [f"{k}={v}" for k, v in envs.items()]
        elif not isinstance(envs, list):
            envs = []
        if envs:
            lines.append(f"env = {toml_list(envs)}")
    else:
        # Yuki always syncs via an image; without one, fall back to script.
        lines.append('provider = "script"')
        lines.append('command = "true"  # no image in Yuki config — fill me in"')

    storage = repo.get("storageDir")
    if storage:
        lines.append(f"storage = {toml_str(str(storage))}")
    else:
        raise SystemExit(f"repo {name}: missing storageDir")
    return "\n".join(lines) + "\n"


def toml_list(items):
    return "[" + ", ".join(toml_str(i) for i in items) + "]"


def main():
    ap = argparse.ArgumentParser(description="migrate Yuki repo YAMLs to Synora config")
    ap.add_argument("dir", help="Yuki repo-config directory (or a single .yaml file)")
    ap.add_argument("-o", "--out", default="config", help="output config dir (default: config)")
    ap.add_argument("--log-dir", default="/var/log/synora", help="synora log dir")
    ap.add_argument("--db", default="data/synora.db", help="synora sqlite db path")
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
        target.write_text(render_job(repo), encoding="utf-8")
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
