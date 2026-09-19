# Repository discovery and metadata consistency

APT/YUM version inventories are read at runtime, not maintained as release-number
arrays. Failed or empty discovery aborts the job before package deletion.

| Consumers | Discovery source |
| --- | --- |
| All `@ubuntu-lts` / `@debian-current` templates (Chef, GitLab, Wine, Proxmox, ROS, Adoptium, etc.) | Debian distro-info-data maintained release CSVs; release dates and rolling release counts (three Debian releases by default) |
| All `@rhel-current` / `@fedora-current` templates | AlmaLinux major-release directory / Fedora Bodhi current Fedora releases, followed by upstream repomd probes |
| Elastic | Official artifacts versions API; stable major branches |
| MySQL | Upstream APT suites and Release components/architectures; YUM component and OS directories |
| MongoDB | Paginated S3 directory discovery; per-suite Release metadata |
| CVMFS, Erlang, Debian ELTS, OpenMediaVault | Upstream suite indexes (GitHub Pages uses repository contents API) |
| LLVM | Maintained distro codenames and upstream `conf/distributions`; no numbered fallback |
| VirtualBox standalone packages | Major versions discovered from upstream directory |
| Adoptium standalone packages | Official available LTS releases API |

Stable channel labels such as `stable`, `main`, `latest`, `jdk1.8`, or
`termux-packages-24` are repository/path identifiers, not release inventories.
They remain unchanged. Discovery does not remove old standalone release trees.

`apt-sync.py` accepts `@auto` for suites, components and architectures. Nested
paths can contain `@auto`, such as `@auto/mongodb-org/@auto`. Components and
architectures come from the relevant suite's Release file, not a global matrix.
`yum-sync.py` accepts `@auto` for template fields and probes `repomd.xml` using
GET, distinguishing unavailable combinations (404/410) from network/server errors.

YUM statistics select primary metadata from `repomd.xml` and support zstd, xz,
gzip and bzip2. Metadata generation/download failures now fail the run. Each
repository retains its own upstream URL when downloading metadata.

The native HTTP provider reads RPM metadata from `repomd.xml`, rather than stale
HTML directory listings. It stages the exact manifest and publishes it only after
referenced downloads succeed. Downloaded files missing with HTTP 404/410 (including referenced metadata)
produce success_with_warnings. Any missing file holds back the staged RPM manifest
and suppresses deletion. Directory traversal, malformed manifests, network errors,
and other transfer errors still fail. Upstream changes during a long run
may still require retrying against a fresh manifest.

Validation: `python3 -m unittest discover -s synora-scripts/tests`, shell syntax
checks, and `cargo test -p httpfetch --lib`.

APT deletion is suppressed when local Release files describe suites outside the
current run, preserving packages referenced by retained distribution indexes.

## Per-job selection

Every APT/YUM script uses the same optional environment policy. Unset/empty lists
select all combinations discovered by that script; there is no implicit new
exclusion. Comma-separated shell-style patterns are supported; exclusions win.

| Variable | Select / exclude |
| --- | --- |
| `SYNC_VERSIONS`, `SYNC_EXCLUDE_VERSIONS` | APT suite (including nested MongoDB release path), or YUM OS version |
| `SYNC_ARCHES`, `SYNC_EXCLUDE_ARCHES` | APT architecture / YUM repository architecture |
| `SYNC_COMPONENTS`, `SYNC_EXCLUDE_COMPONENTS` | Component, including YUM product releases such as Elastic `8.x` and MongoDB `8.0` |

Example Synora job configuration (merge with any existing `env` entries):

```toml
env = ["SYNC_VERSIONS=bookworm,trixie", "SYNC_ARCHES=amd64,arm64", "SYNC_EXCLUDE_ARCHES=arm64"]
```

For each discovered combination logs show `version=... architecture=...
component=... sync=yes/no`. Run `apt-sync.py --dry-run ...` or `yum-sync.py
--dry-run ...` with the same arguments/environment to inspect the upstream
inventory and selections without downloading packages. Discovery errors still
fail; an explicitly empty selection preserves existing data. APT deletion is
suppressed while filters are active so excluded suites/architectures keep their
packages. YUM only processes selected repository directories.
