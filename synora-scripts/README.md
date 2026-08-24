# synora-scripts

Mirror sync scripts for Synora `provider = "script"` and `provider = "docker"` jobs.

They run inside the `synora-scripts` image (`docker run synora-scripts:latest`).
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
| `SYNORA_STATUS=success\|failed` | optional explicit outcome |
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
