# synora-scripts

Mirror sync scripts for Synora `provider = "script"` and `provider = "docker"` jobs.

They run inside the `synora-scripts` image (`docker run synora-scripts:latest`).

The AOSP script uses four parallel `repo sync` jobs by default. Set
`AOSP_SYNC_JOBS` on the job to override the concurrency.
Git jobs (`provider = "git"`) use the same image. Script jobs can keep
`provider = "script"` (worker wraps them in `scripts_image`) or use an
explicit docker job:

```toml
provider = "docker"
image = "synora-scripts:latest"
docker_command = ["/usr/lib/synora/scripts/mysql.sh"]
```

Tunasync `command` + `docker_image` jobs keep this docker form
(`AOSP`, `docker-ce`, `github-release`, `rubygems`, yum/apt scripts, …).
`git.sh` stays `provider = "git"` (still runs in the same image).
Job commands stay `/usr/lib/synora/scripts/<name>`.

## Environment

| Variable | Meaning |
|---|---|
| `SYNORA_JOB` | job name |
| `SYNORA_UPSTREAM` | upstream URL |
| `SYNORA_STORAGE` | working directory (bind-mounted host path) |
| `SYNORA_LOG_DIR` | per-job log directory |
| `SYNORA_RUN_ID` | current run id |
| `SYNORA_API` | manager API URL as seen from the job (`[worker].manager`; docker rewrites loopback to `172.17.0.1`) |
| `SYNORA_PROXY` / `ALL_PROXY` / `HTTP(S)_PROXY` | assigned proxy |
| `SYNORA_SIZE=` | bytes, printed on stdout when known |
| `SYNORA_STATUS=success\|success_with_warnings\|failed` | optional explicit outcome |
| `MIRROR_BASE_URL` | rustup: public URL written into manifests |
| `RUSTUP_TARGETS` | rustup: comma-separated rustc targets (default: Tier 1, no i686) |
| `RUSTUP_GC` | rustup: nightly retention days (default 30) |
| `RUSTUP_MIRROR_TIMEOUT_SECS` | rustup: per-file client timeout (default 21600) |

HTTP directory mirrors use Synora's native `http` provider, not `tsumugu.sh`.

The worker injects `ALL_PROXY` / `HTTP(S)_PROXY` and starts each job with
`docker --init --entrypoint <script>` so git/repo children are reaped.
Python scripts that need CONNECT for `http://` URLs call
`helpers.http_connect.enable()`. `proxmox.sh` and `virtualbox.sh` use
`helpers/http_connect_proxy.py` as a compatibility adapter for older
CONNECT-only managers; it is not PID 1. `github-release.py` reads
`github-release.json` from the script directory. `debian.sh` / `kali.sh`
generate an ftpsync config from `SYNORA_UPSTREAM` and `SYNORA_STORAGE`.


## Image

`synora-scripts/Dockerfile` builds the general script runtime. Dedicated
RubyGems, Rustup, Nix, Yukina, Shadowmire, and ftpsync runtimes are described in
[deploy/docker](../deploy/docker/README.md). TUNA-only helpers such as
`rustup-tuna-proxy.py` are not shipped.

```sh
docker build -t synora-scripts:latest synora-scripts
# or
scripts/build-synora-scripts-image.sh
```

Apt stays direct. Optional HTTPS fetch proxy for git/cargo/gem/pip/curl:

```sh
scripts/build-synora-scripts-image.sh --proxy "$HTTPS_PROXY"
```

The general image includes git, Python, dnf, createrepo_c, `repo`, and
the shared synchronization scripts. Specialized tools live in their dedicated
images.

## Acknowledgements

These scripts started as [tunasync-scripts](https://github.com/tuna/tunasync-scripts)
from TUNA (Tsinghua University TUNA Association). Synora rewrites them to
`SYNORA_*` environment variables and runs them in its own image. Thank you
to TUNA and the tunasync-scripts authors and maintainers.

## Channel and recipe mirrors

The Nix channel port follows `tuna/tunasync-scripts` at
`b7e131dc4a1c4711f84cbbe6c6168f0f95ac3fbc`. Its dedicated image includes the
matching Nix 2.3.2 CLI and MinIO 5 API used by that script. Set
`NIX_MIRROR_RETAIN_DAYS=14` (default) and `MIRROR_BASE_URL` to the public
`/nix-channels` URL. Garbage collection retains current published channels and
recent releases, aborts before deletion if closure calculation fails, and keeps
partial downloads while an interrupted update resumes. Historical releases no
longer referenced after the retention interval are removed. Current channels can
therefore reference cache objects older than 14 days.

The general image installs the PyBOMBS mirror helper pinned at
`scateu/pybombs-mirror@d7a8a9925c0b91dc9695a31a07b62eff8546909e`.
Run `pybombs.sh` with `MIRROR_BASE_URL` pointing to the public `/pybombs` URL.
The wrapper generates Git HTTP indexes, checks that usable recipes were produced,
and reports failed source URLs as warnings. No usable recipe repository is a
failure. The upstream helper does not implement SVN mirroring; these recipes
keep their original upstream URLs and are included in the warning report.

Git repositories and complete rsync mirrors can use native Synora providers
instead of wrapping the equivalent TUNA scripts. InfluxData RPM packages now use its
canonical `stable/<architecture>/main` repository, with architectures discovered
from the upstream index. APT suites, components and architectures are likewise
discovered rather than inferred from OS release lists. RPM content is stored under
`yum/stable-<architecture>`; old `yum/el*` directories are left in place.

## Official-upstream recovery

XanMod's official server serves Release/Packages files but does not expose a
`dists/` directory listing. `xanmod.sh` uses the codenames currently advertised on
xanmod.org (`@xanmod`) and discovers each suite's components and architectures
from its Release file. Failure to discover the list is fatal; no static version
fallback is used.

MongoDB's historical RPM indexes can reference primary metadata that no longer
exists in its public S3 bucket. YUM synchronization now runs repositories
individually, so a failing branch does not block attempts for the others. For
this specific MongoDB condition, it enumerates the official repository's RPM
objects, checks sizes, single-part S3 ETags and downloaded RPM digests, then
rebuilds metadata with createrepo. Repaired branches produce a warning; incomplete
inventory, corrupt packages and other repository failures still fail the run.
Existing packages are retained during recovery. Other upstreams do not use this
fallback. `MONGO_RPM_THREADS` controls recovery downloads (default 4, maximum 16).
MongoDB also attempts both APT families even when YUM fails, and keeps existing
x86_64 repository names while giving other architectures separate directories.

## Shared repository defaults

`repository-defaults.toml` is the common configuration for every APT/YUM wrapper.
The wrappers select their own repository profile; job files do not need a profile
flag or repeated architecture lists. Protocol defaults are inherited, then
repository and URL-path exceptions are applied. Existing `SYNC_APT_ARCHES`,
`SYNC_YUM_ARCHES`, `*_BY_PATH`, `SYNC_VERSIONS`, and `SYNC_COMPONENTS` remain
available for explicit job overrides. `SYNC_EXCLUDE_*` always wins.

The `[discovery]` table controls shared release windows: the latest two released
RHEL-compatible/Fedora versions, three Ubuntu LTS/Debian releases, and the
`debian-latest2`/`debian-latest` subsets. Discovery uses official inventories;
RHEL directory aliases such as `9Server` and minor-version duplicates are ignored.
Upstream software/component discovery remains enabled. MongoDB's selected product
versions live once in `[groups].mongodb-releases`, referenced by both APT and YUM.
No TUNA scripts or policies are fetched at runtime.

To pin a shared distribution group, add it to `[groups]`, for example:

```toml
[groups]
rhel-current = ["9", "10"]
fedora-current = ["41", "42"]
```

These arrays replace discovery for that group. Otherwise the corresponding
`[discovery]` rule remains automatic. `SYNC_DEFAULTS_FILE` can point to a complete
replacement TOML file mounted read-only into a container; by default the file
shipped in the image is used. Invalid configuration stops the sync.

Production jobs normally need only actual exceptions:

```toml
# InfluxData
env = ["SYNC_EXCLUDE_VERSIONS=stable"]

# Proxmox
env = ["SYNC_EXCLUDE_COMPONENTS=pvetest"]
```

Overrides replace inherited include lists; exclusions then further restrict them.
An explicit empty include clears an inherited restriction. Architecture overrides
must be nonempty (use `@auto` for upstream discovery). Active selection preserves
excluded local content during cleanup so shared packages are not deleted merely
because a different scope was selected. Review obsolete branches separately.

The `[packages]` section defines common debug/test package-name segments and file
suffixes. APT/YUM use it directly. Generate the equivalent rsync filter file with:

```sh
python3 synora-scripts/helpers/package_policy.py --rsync-filters > /tmp/package-filters.rules
install -m 644 /tmp/package-filters.rules /etc/synora/excludes/package-filters.rules
```

Install the generated file on every worker using it. Existing rsync jobs continue
to reference this one file. Receiver-side `R` rules are intentional: excluded
packages already on disk remain eligible for deletion. Repository-specific
exclusions, such as unrelated archive directories, remain in each job. Normal
`-dev` and `-devel` packages are retained. CI checks the committed generated file
against the common package configuration.
