# Synora 0.2.0

## Reliability and safety

- Worker claims now use fencing tokens. Lease renewal, progress and completion
  reject stale assignments, and workers cancel execution when their lease is lost.
- SQLite and PostgreSQL enforce one active run per job, with concurrent assignment
  tests for both backends. Runs record the configuration generation that created them.
- Independent mirrors on one ZFS/Btrfs backend can sync concurrently. Hierarchical
  storage locks exclude overlapping paths and whole-backend rollback; subprocesses
  inherit the locks so a worker exit does not immediately unlock active writers.
- ZFS storage validates its actual mountpoint and mounted state. Btrfs rollback
  prepares and atomically exchanges subvolumes, retaining the old tree for inspection.
- Snapshot names are unique, and requested snapshot failures fail the run by default.
- HTTP transfer/listing failures are reported as failures. Rsync exit 23 now fails
  by default; exit 24 remains accepted, with explicit success-code overrides supported.
- External control commands have timeout and output limits. Log retention, safe
  filesystem paths, resource accounting and poisoned-lock handling are hardened.

## Security and TUI

- Metrics require authentication by default. Configuration reload and sensitive
  hook/provider/storage edits have separate permissions.
- Docker execution drops capabilities and rejects unsafe host access and extra
  writable mounts. Configuration expansion and loading have bounded limits.
- Fix path traversal in log access, cleanup, HTTP mirrors and the Rustup proxy script.
- TUI editing preserves typed configuration values and reports invalid input;
  navigation, filtering and small-terminal layouts are improved.

## Builds and upgrading

- CI runs formatting, strict Clippy, workspace tests, PostgreSQL concurrency tests
  and builds with Rust 1.98.0. Release binaries are built on Ubuntu 22.04 and checked
  for a glibc requirement no newer than 2.35; release caches are isolated accordingly.
- Drain all old workers before upgrading the manager and workers together: the
  fenced assignment protocol is incompatible with older workers. Back up the
  database and configuration before the schema migration.
- Review Docker options, snapshot failure policy, rsync success codes, metrics
  authentication and API permissions when upgrading existing configurations.
- Shared-storage takeover still requires infrastructure fencing and coherent
  filesystem locks. Automatic retry after worker loss is opt-in; application
  tokens alone cannot stop an isolated host or detached container from writing.
