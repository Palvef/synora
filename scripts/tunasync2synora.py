#!/usr/bin/env python3
"""tunasync2synora — migrate a tunasync workers.conf to Synora job TOML files.

Reads a tunasync worker config (INI: [global], [manager], [[mirrors]] sections,
see tunasync docs/configs.md) and writes one TOML file per mirror into the
target directory, plus a main synora.toml with daemon settings + include.

Mapping:
  provider "rsync"            -> provider = "rsync" (+ options from rsync_options,
                                 success_exit_codes from rsync_success_exit_codes /
                                 success_exit_codes)
  provider "command"          -> provider = "script", command = <command>
                                 (env vars TUNASYNC_* are injected by Synora at
                                 run time — scripts keep working unchanged)
  interval (minutes)          -> schedule = "interval", every = "<N>m"
                                 (Synora is anchor-based/no-drift, unlike tunasync's
                                 completion+interval)
  timeout / retry             -> timeout / retry
  exec_on_success / exec_on_failure -> [jobs.hooks] on_success / on_failure
  size_pattern                -> dropped (Synora reads SYNORA_SIZE= lines; add
                                 `echo SYNORA_SIZE=...` to the script if needed)
  docker_image                -> provider = "docker" with image/volumes/env
  env                         -> job-level env is injected by the provider;
                                 docker env entries are copied to the docker block

Usage:
  python3 tunasync2synora.py workers.conf -o config/ [--log-dir /var/log/synora]

Only the Python standard library. Offline-safe.
"""

import argparse
import re
import configparser
import glob
import os
import sys
from pathlib import Path

TOML_ESCAPE = str.maketrans({"\\": "\\\\", '"': '\\"'})


def toml_str(s):
    return '"' + str(s).translate(TOML_ESCAPE) + '"'


def toml_list(items):
    return "[" + ", ".join(toml_str(i) for i in items) + "]"


def parse_list(value):
    """tunasync array syntax: ["a","b"] or bare values — be lenient."""
    value = (value or "").strip()
    if not value:
        return []
    if value.startswith("["):
        import ast
        try:
            return [str(x) for x in ast.literal_eval(value)]
        except (ValueError, SyntaxError):
            pass
    return [v.strip().strip('"\'') for v in value.split(",") if v.strip()]


def render_job(m, log_dir, storage_dir):
    lines = ["[[jobs]]", f"name = {toml_str(m['name'])}", "enabled = true", ""]
    provider = m.get("provider", "rsync")

    # --- schedule: tunasync interval (minutes) → Synora no-drift interval ---
    interval = m.get("interval")
    if interval:
        lines.append('schedule = "interval"')
        lines.append(f"every = {toml_str(f'{int(interval)}m')}")
    else:
        lines.append('schedule = "manual"  # no interval configured in tunasync')

    lines.append("")

    # --- provider ---
    if provider == "rsync":
        lines.append('provider = "rsync"')
        lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
        options = parse_list(m.get("rsync_options"))
        exclude = m.get("exclude_file")
        if exclude:
            options.append(f"--exclude-from={exclude}")
        if options:
            lines.append(f"options = {toml_list(options)}")
        codes = parse_list(m.get("rsync_success_exit_codes")) or parse_list(
            m.get("success_exit_codes")
        )
        if codes:
            lines.append(f"success_exit_codes = {codes}")
    elif provider == "command":
        lines.append('provider = "script"')
        lines.append(f"command = {toml_str(m.get('command', ''))}")
        lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
        if m.get("fail_on_match"):
            lines.append(f"fail_on_match = {toml_str(m['fail_on_match'])}")
    elif provider in ("docker", "two-stage-rsync"):
        image = m.get("docker_image", "ustcmirror/rsync:latest")
        lines.append('provider = "docker"')
        lines.append(f"image = {toml_str(image)}")
        lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
        env = []
        for e in parse_list(m.get("env")):
            env.append(e)
        if env:
            lines.append(f"env = {toml_list(env)}")
        volumes = ["/data"]  # storage is mounted at /data by Synora
        volumes.extend(parse_list(m.get("docker_volumes")))
        if len(volumes) > 1:
            lines.append(f"volumes = {toml_list(volumes[1:])}")
    else:
        raise SystemExit(f"unsupported provider {provider!r} for mirror {m['name']}")

    # --- storage path: tunasync mirror_dir/subdir composition ---
    mirror_dir = m.get("mirror_dir") or storage_dir
    subdir = m.get("mirror_subdir")
    if mirror_dir and subdir and str(mirror_dir) != str(m.get("mirror_dir")):
        pass
    if m.get("mirror_dir"):
        path = m["mirror_dir"]
    elif subdir:
        path = os.path.join(storage_dir, subdir, m["name"])
    else:
        path = os.path.join(storage_dir, m["name"])
    lines.append(f"storage = {toml_str(path)}")

    # --- retry / timeout ---
    if m.get("retry"):
        lines.append(f"retry = {int(m['retry'])}")
    if m.get("timeout"):
        lines.append(f"timeout = {int(m['timeout'])}")

    # --- hooks (tunasync exec_on_*) ---
    hooks = {}
    for src, dst in (("exec_on_success", "on_success"), ("exec_on_failure", "on_failure")):
        items = parse_list(m.get(src))
        if items:
            hooks[dst] = items
    if hooks:
        lines.append("")
        lines.append("[jobs.hooks]")
        for dst, items in hooks.items():
            lines.append(f"{dst} = {toml_list(items)}")

    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description="migrate tunasync workers.conf to Synora config")
    ap.add_argument("conf", help="tunasync workers.conf path")
    ap.add_argument("-o", "--out", default="config", help="output config dir (default: config)")
    ap.add_argument("--log-dir", default="/var/log/synora", help="synora log dir")
    ap.add_argument("--db", default="data/synora.db", help="synora sqlite db path")
    args = ap.parse_args()

def preprocess(text):
    """Join multi-line arrays (`x = [\n "a",\n]`) onto one line —
    configparser cannot read those."""
    lines = []
    buf = None
    for line in text.splitlines():
        if buf is not None:
            buf += " " + line.strip()
            if line.rstrip().endswith("]") or line.rstrip().endswith("}"):
                lines.append(buf)
                buf = None
        elif line.rstrip().endswith("[") or line.rstrip().endswith("{"):
            buf = line
        else:
            lines.append(line)
    if buf is not None:
        lines.append(buf)
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description="migrate tunasync workers.conf to Synora config")
    ap.add_argument("conf", help="tunasync workers.conf path")
    ap.add_argument("-o", "--out", default="config", help="output config dir (default: config)")
    ap.add_argument("--log-dir", default="/var/log/synora", help="synora log dir")
    ap.add_argument("--db", default="data/synora.db", help="synora sqlite db path")
    args = ap.parse_args()

    cp = configparser.ConfigParser(interpolation=None, strict=False)
    cp.optionxform = str  # preserve key case
    with open(args.conf, encoding="utf-8") as f:
        cp.read_string(preprocess(f.read()))

    if not cp.has_section("global"):
        raise SystemExit(f"{args.conf}: missing [global] section — not a tunasync worker conf?")

    global_sec = dict(cp.items("global"))
    storage_dir = global_sec.get("mirror_dir", "/srv/mirror").strip('"').strip("'")

    out = Path(args.out)
    jobs_dir = out / "jobs"
    jobs_dir.mkdir(parents=True, exist_ok=True)

    count = 0
    # configparser merges repeated [[mirrors]] sections (same-name keys
    # overwrite each other), so parse mirror blocks manually.
    mirrors = []
    with open(args.conf, encoding="utf-8") as f:
        raw = preprocess(f.read())
    blocks = re.split(r"\[\[mirrors\]\]", raw)
    # the first block is everything before the first [[mirrors]] (global etc.)
    for block in blocks[1:]:
        m = {}
        for line in block.splitlines():
            line = line.split("#", 1)[0].strip()
            if not line or line.startswith("["):
                if m and line.startswith("["):
                    break  # next section header ends this mirror block
                continue
            if "=" not in line:
                continue
            k, _, v = line.partition("=")
            m[k.strip()] = v.strip().strip('"').strip("'")
        if m.get("name"):
            mirrors.append(m)
    for m in mirrors:
        target = jobs_dir / f"{m['name']}.toml"
        target.write_text(render_job(m, args.log_dir, storage_dir), encoding="utf-8")
        print(f"  {m['name']}: {target}")
        count += 1

    if count == 0:
        print(f"warning: no [[mirrors]] entries found in {args.conf}")

    main_toml = f'''# Generated by tunasync2synora from {args.conf}
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
    print("next: `synora check -c {}/synora.toml`".format(out))


if __name__ == "__main__":
    main()
