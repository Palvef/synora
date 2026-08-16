# Synora

A modern, high-performance mirror synchronization engine in Rust.
Synora decides **when** to sync, **where** to sync, **through which network
path**, **with which tool**, and **how to recover** — the actual data movement
is delegated to rsync, scripts, or Docker containers (tunasync/Yuki-style).

Rust · Mirror Sync · Cron · Multi Worker · Docker · Proxy · IPv4/IPv6 ·
ZFS/Btrfs · Prometheus · TUI

## Features

- **No-drift scheduling**: cron / daily / weekly / fixed interval with a
  persistent anchor — next runs are computed from the wall clock, never from
  "last run end + interval" (misfire policies: skip / run-immediately / run-next).
- **Three providers**: rsync (tunasync-aligned defaults, `success_exit_codes`
  23/24), script (SYNORA_* + TUNASYNC_* env for tunasync-scripts compatibility,
  `SYNORA_SIZE=` size reporting), docker (`docker run`, storage mounted at /data).
- **Single machine or distributed**: `synora start` runs standalone (SQLite);
  `synora-manager` + N × `synora-worker` form a pull-model cluster (workers
  register, heartbeat every 15 s, claim assigned runs). PostgreSQL optional.
- **Crash safety**: every run has a 60 s lease; lease expiry → LOST →
  automatic re-dispatch (`on_worker_lost = "retry"`). No run stays RUNNING
  forever.
- **Hot reload**: SIGHUP / `synora reload` / `POST /api/v1/reload` — job and
  schedule changes apply live; invalid or non-reloadable changes are rejected
  as a whole.
- **Security**: Bearer-token API with RBAC permission keys, plain HTTP or
  TLS/mTLS (tunasync-style `[api.tls]` + worker `ca_cert`), argv-only process
  execution (no shell string concatenation), constant-time token comparison.
- **Observability**: Prometheus metrics, per-run log files, events table, TUI.
- **Config**: TOML with `include` (glob/nested/cycle-detected), `${VAR}`
  expansion, `file:line` validation via `synora check`.
- **Migration**: `scripts/tunasync2synora.py` and `scripts/yuki2synora.py`
  convert existing tunasync / Yuki configs in one shot.

## Quick start

```sh
# validate a config (file:line errors)
synora check -c examples/simple.toml

# run the standalone daemon (SQLite + scheduler + /metrics on 127.0.0.1:8100)
synora start -c examples/simple.toml

# trigger one job now
synora run ubuntu -c examples/simple.toml

# status / logs / cancel / reload
synora status -c examples/simple.toml
synora logs ubuntu -c examples/simple.toml
synora stop slow -c examples/simple.toml
synora reload -c examples/simple.toml

# distributed mode
synora-manager -c config/synora.toml     # manager (API + scheduler)
synora-worker  -c config/worker1.toml    # one worker per host
synora worker list -c config/synora.toml
synora-tui -c config/synora.toml         # terminal console
```

## Configuration

```toml
include = ["jobs/*.toml"]        # globs, nesting, cycle detection

[daemon]
max_concurrency = 16
log_dir = "/var/log/synora"

[daemon.db]
kind = "sqlite"                  # or "postgres" + url = "postgres://..."
path = "data/synora.db"

[api]
listen = "127.0.0.1:8100"

[api.tls]                        # optional; client_ca enables mTLS
cert = "/etc/synora/cert.pem"
key  = "/etc/synora/key.pem"
# client_ca = "/etc/synora/client-ca.pem"

[[api.tokens]]
name = "admin"
token = "change-me"
role = "admin"                   # admin | operator | viewer
```

A job (`jobs/*.toml`):

```toml
[[jobs]]
name = "ubuntu"
enabled = true
schedule = "cron"                # cron | daily | weekly | interval | manual | startup
cron = "0 */6 * * *"
timezone = "Asia/Shanghai"

provider = "rsync"               # rsync | script | docker
upstream = "rsync://archive.ubuntu.com/ubuntu/"
storage = "/srv/mirror/ubuntu"
options = ["--delete", "--delay-updates"]
success_exit_codes = [23, 24]

retry = 3
retry_delay = "5m"
timeout = "2h"
worker = "g1"                    # worker id, group label, or omitted (auto)
resources = ["large-disk"]       # matched against worker labels
statistics = "provider"          # provider | filesystem

[jobs.hooks]
on_success = ["/opt/scripts/publish.sh"]

[jobs.safety]
max_delete_files = 50000
max_delete_ratio = 0.30
```

Worker (`worker1.toml`):

```toml
[worker]
manager = "https://synora.example.org:8100"
token = "worker-token"
labels = ["g1", "zfs"]
ca_cert = "/etc/synora/ca.pem"   # optional, verifies the manager's TLS
max_concurrency = 8
```

## REST API

All endpoints under `/api/v1`, `Authorization: Bearer <token>`. Roles:
admin / operator / viewer; permission keys: `jobs.read`, `jobs.write`,
`runs.manage`, `workers.read`, `workers.write`, `logs.read`.

| Method | Path | Permission | Purpose |
|---|---|---|---|
| POST | `/workers/register` | runs.manage | worker registration → worker_id |
| POST | `/workers/{id}/heartbeat` | runs.manage | heartbeat + lease refresh; returns run assignment / cancel request |
| POST | `/runs/{id}/claim` | runs.manage | atomic claim (409 if taken) |
| POST | `/runs/{id}/complete` | runs.manage | success / failed / cancelled report |
| POST | `/workers/{id}/drain` | workers.write | stop accepting new runs |
| DELETE | `/workers/{id}` | workers.write | unregister (only when idle) |
| GET | `/jobs` | jobs.read | jobs with status/next_run/size |
| POST | `/jobs/{name}/run` | jobs.write | trigger a run now |
| POST | `/jobs/{name}/stop` | jobs.write | cancel (worker cancels on next heartbeat) |
| GET | `/jobs/{name}/history` | jobs.read | run history |
| GET | `/jobs/{name}/logs?tail=200` | logs.read | tail of current.log |
| GET | `/workers` | workers.read | worker list |
| POST | `/reload` | jobs.write | hot-reload config |
| GET | `/metrics` | (open) | Prometheus text format |

## Metrics

`synora_job_status{job,worker}` (gauge: 0 pending, 4 running, 5 success,
6 failed, 9 cancelled, 10 lost), `synora_job_runs_total`, `failures_total`,
`retries_total`, `duration_seconds`, `last_success/start/end_timestamp`,
`next_run_timestamp`, `bytes_transferred_total`, `repository_size_bytes`,
`synora_worker_status`, `synora_worker_jobs_running`.

## Repository size

Priority (spec §17): provider report (`--stats` / `SYNORA_SIZE=`) → script
output → filesystem walk when `statistics = "filesystem"`. Stored as raw
bytes + human form (KiB/MiB/GiB/TiB).

## Design principles

Scheduler ↔ Executor decoupled · Provider ↔ Storage decoupled ·
Manager ↔ Worker decoupled · DB is the source of truth · all states
observable · all tasks recoverable. See the full spec in the project plan
for phases beyond P0+P1 (proxy/egress, ZFS/Btrfs snapshots, HTTP provider,
HA, Provider SDK).

## Development

```sh
cargo build --workspace     # offline after first fetch
cargo test --workspace      # 45+ tests: scheduler no-drift, config, parser, engine
cargo clippy --workspace
synora check -c examples/simple.toml
```

Workspace crates: `synora-core` (domain types, no-drift schedule math,
metrics), `config`, `db` (SQLite/PostgreSQL), `provider` (rsync/script/docker),
`engine` (scheduler/executor), `api` (DTOs + client), `manager`, `worker`,
`cli`, `tui`, `nginx` (directory-index parser).

## License

TBD
