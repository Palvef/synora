#!/usr/bin/env python3
"""tunasync2synora — migrate a tunasync workers.conf to Synora job TOML files.

Reads a tunasync worker config (INI: [global], [manager], [[mirrors]] sections,
see tunasync docs/configs.md) and writes one TOML file per mirror into the
target directory, plus a main synora.toml with daemon settings + include.

Mapping:
  provider "rsync"            -> provider = "rsync" (+ options from rsync_options,
                                 success_exit_codes from rsync_success_exit_codes /
                                 success_exit_codes)
  provider "command"          -> provider = "docker" when docker_image is set
                                 (synora-scripts:latest + docker_command),
                                 else provider = "script"
                                 git.sh stays provider = "git"
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
import shlex
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


def strip_tsumugu_threads(env_list):
    """TUNASYNC_TSUMUGU_THREADS=N is tsumugu's download-concurrency knob; it
    leaves the env list and becomes the http provider's `threads` field
    instead. Returns (threads_value_or_None, env_without_it)."""
    threads = None
    kept = []
    for e in env_list:
        k, _, v = e.partition("=")
        if k.strip().upper() == "TUNASYNC_TSUMUGU_THREADS":
            threads = v.strip()
        else:
            kept.append(e)
    return threads, kept


def render_job(m, log_dir, storage_dir, global_docker_volumes, global_interval):
    lines = ["[[jobs]]", f"name = {toml_str(m['name'])}", "enabled = true", ""]
    provider = m.get("provider", "rsync")
    # tsumugu's concurrency knob must not linger in the docker env: it
    # becomes the job-level `threads` field (http provider).
    threads, stripped_env = strip_tsumugu_threads(
        parse_list(m.get("env")) + m.get("env_table", [])
    )

    # --- schedule: tunasync interval (minutes) → Synora no-drift interval ---
    # A mirror without its own interval inherits the worker-level global
    # `interval` (tunasync semantics), not "manual".
    interval = m.get("interval")
    if interval:
        lines.append('schedule = "interval"')
        lines.append(f"every = {toml_str(f'{int(interval)}m')}")
    elif global_interval:
        lines.append('schedule = "interval"')
        lines.append(f"every = {toml_str(f'{int(global_interval)}m')}")
        lines.append("  # inherited from the tunasync global interval")
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
        options = [
            o.replace("/etc/tunasync/syncpassword/", "/etc/synora/syncpassword/")
            .replace("/etc/tunasync/excludes/", "/etc/synora/excludes/")
            for o in options
        ]
        if options:
            lines.append(f"options = {toml_list(options)}")
        codes = parse_list(m.get("rsync_success_exit_codes")) or parse_list(
            m.get("success_exit_codes")
        )
        if codes:
            lines.append(f"success_exit_codes = {codes}")
    elif provider == "command":
        command = m.get("command", "") or ""
        command = command.replace("/home/tunasync-scripts/", "/usr/lib/synora/scripts/")
        env = list(stripped_env)
        proxy_env = [e for e in env if any(k in e.upper() for k in ("PROXY",))]
        env = [e for e in env if e not in proxy_env]
        # Native git jobs: tunasync wrapped git.sh in a container.
        if command.rstrip().endswith("/git.sh") or command.strip() in (
            "/usr/lib/synora/scripts/git.sh",
            "/home/tunasync-scripts/git.sh",
            "git.sh",
        ):
            # Native git still runs inside scripts_image.
            lines.append('provider = "git"')
            lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
        elif m.get("docker_image"):
            # tunasync command+docker_image jobs stay docker in Synora.
            args = shlex.split(command) if command.strip() else []
            lines.append('provider = "docker"')
            lines.append('image = "synora-scripts:latest"')
            if args:
                lines.append(f"docker_command = {toml_list(args)}")
            lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
            if env:
                lines.append(f"env = {toml_list(env)}")
            if m.get("fail_on_match"):
                lines.append(f"fail_on_match = {toml_str(m['fail_on_match'])}")
        else:
            lines.append('provider = "script"')
            lines.append(f"command = {toml_str(command)}")
            lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
            if env:
                lines.append(f"env = {toml_list(env)}")
            if m.get("fail_on_match"):
                lines.append(f"fail_on_match = {toml_str(m['fail_on_match'])}")
        if proxy_env:
            lines.append('proxy = "cf-warp"')
    elif provider == "two-stage-rsync":
        # Native two-stage rsync (stage 1 subset profile, stage 2 full sync).
        lines.append('provider = "two-stage-rsync"')
        lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
        sp = m.get("stage1_profile") or "debian"
        lines.append(f"stage1_profile = {toml_str(sp)}")
        # Same extra rsync fields as the plain rsync branch.
        opts = parse_list(m.get("rsync_options")) or parse_list(m.get("options"))
        if opts:
            lines.append(f"options = {toml_list(opts)}")
        excl = parse_list(m.get("exclude"))
        if excl:
            lines.append(f"exclude = {toml_list(excl)}")
        codes = m.get("success_exit_codes")
        if codes:
            lines.append(f"success_exit_codes = {toml_str(codes)}")
    elif provider in ("docker",):
        image = m.get("docker_image", "ustcmirror/rsync:latest")
        lines.append('provider = "docker"')
        lines.append(f"image = {toml_str(image)}")
        lines.append(f"upstream = {toml_str(m.get('upstream', ''))}")
        env = list(stripped_env)
        # Fixed proxy envs migrate to the worker docker-bridge HTTP proxy.
        proxy_env = [e for e in env if any(k in e.upper() for k in ("PROXY",))]
        if proxy_env:
            lines.append('proxy = "cf-warp"')
        env = [e for e in env if e not in proxy_env]
        if env:
            lines.append(f"env = {toml_list(env)}")
        volumes = ["/data"]  # storage is mounted at /data by Synora
        volumes.extend(parse_list(m.get("docker_volumes")))
        if len(volumes) > 1:
            lines.append(f"volumes = {toml_list(volumes[1:])}")
    else:
        raise SystemExit(f"unsupported provider {provider!r} for mirror {m['name']}")

    # tsumugu download concurrency -> http provider `threads` (job-level;
    # ignored by non-http providers).
    if threads is not None:
        lines.append(f"threads = {threads}")

    # --- storage: relative path + storage section reference ---------------
    # tunasync semantics: the mirror lives under the worker's storage_dir
    # (which differs per machine: /datas here, /data elsewhere). Jobs write
    # the RELATIVE path and reference the worker's [storage.mirror] section,
    # so one config works on every machine's local pool/mountpoint.
    if m.get("mirror_subdir"):
        # tunasync places the mirror at mirror_dir/<mirror_subdir>/<name>
        # (e.g. /datas/git/AOSP, one ZFS dataset per mirror).
        rel = f"{m['mirror_subdir']}/{m['name']}"
    else:
        rel = m["name"]
    lines.append(f"storage = {toml_str(rel)}")
    lines.append('storage_name = "mirror"')

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

    mem = m.get("memory_limit")
    if mem:
        lines.append(f"memory_limit = {toml_str(str(mem))}")

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
    global_interval = global_sec.get("interval")
    zpool = ""
    if cp.has_section("zfs"):
        zpool = dict(cp.items("zfs")).get("zpool", "").strip('"').strip("'")
    # [docker] section: shared volumes added to every docker job (TUNA
    # production mounts the tunasync-scripts checkout read-only).
    global_docker_volumes = []
    if cp.has_section("docker"):
        global_docker_volumes = [
            v.strip().strip('"').strip("'")
            for v in dict(cp.items("docker")).get("volumes", "").replace("[", "").replace("]", "").split(",")
            if v.strip()
        ]

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
        in_env = False
        env_table = []
        for line in block.splitlines():
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            if line.startswith("["):
                if line == "[mirrors.env]" or line.startswith("[mirrors.env]"):
                    in_env = True
                    continue
                if m:
                    break  # next section header ends this mirror block
                continue
            if "=" not in line:
                continue
            k, _, v = line.partition("=")
            k, v = k.strip(), v.strip().strip('"').strip("'")
            if in_env:
                env_table.append(f"{k}={v}")
            else:
                m[k] = v
        if env_table:
            m["env_table"] = env_table
        if m.get("name"):
            mirrors.append(m)
    for m in mirrors:
        target = jobs_dir / f"{m['name']}.toml"
        target.write_text(
            render_job(m, args.log_dir, storage_dir, global_docker_volumes, global_interval),
            encoding="utf-8",
        )
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

# Worker-local mirror storage: the pool/dataset and mountpoint follow the
# tunasync zpool + mirror_dir settings. Jobs reference this section by name
# and write relative paths, so the same job config works on machines with
# different pools/mounts.
[storage.mirror]
kind = "zfs"
pool = {toml_str(zpool or "data")}
dataset = "mirror"
mountpoint = {toml_str(storage_dir)}
auto_create = true
zfs_options = "-o recordsize=1M -o xattr=off -o atime=off -o setuid=off -o exec=off -o devices=off -o sync=disabled -o secondarycache=metadata -o redundant_metadata=most"
'''
    (out / "synora.toml").write_text(main_toml, encoding="utf-8")
    print(f"wrote {out / 'synora.toml'} ({count} job(s))")
    print("next: `synora check -c {}/synora.toml`".format(out))


if __name__ == "__main__":
    main()
