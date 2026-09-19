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
docker_command = ["/usr/lib/synora/scripts/rubygems.sh"]
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

`synora-scripts/Dockerfile` is the image. rustup-mirror is built from
[jiegec/rustup-mirror](https://github.com/jiegec/rustup-mirror); the binary
is not committed. TUNA-only helpers such as `rustup-tuna-proxy.py` are not
shipped.

```sh
docker build -t synora-scripts:latest synora-scripts
# or
scripts/build-synora-scripts-image.sh
```

Apt stays direct. Optional HTTPS fetch proxy for git/cargo/gem/pip/curl:

```sh
scripts/build-synora-scripts-image.sh --proxy "$HTTPS_PROXY"
```

The image includes git, ftpsync (archvsync), python3, dnf, createrepo_c,
awscli, `repo`, rubygems-mirror, and rustup-mirror.

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
