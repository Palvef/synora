# Synora 0.1.2

## New features

- **Two-stage rsync sync**: two passes — a fast stage that publishes a
  subset of the mirror first (per-profile filters, aborts the run on
  failure), then a full pass that syncs everything
- **Operator-triggered runs start immediately**: manually started runs
  get priority over the scheduled backlog, and idle workers poll faster,
  so a forced run no longer waits behind the queue
- **HTTP sync improvements**: unchanged files are never re-downloaded
  (size/mtime comparison); the download concurrency is configurable
  (`threads` field, default 8); symlinks shown by fancyindex listings are
  mirrored locally (idempotent, existing paths are never overwritten);
  live progress lines during both planning and downloading
- **Live logs for every sync method**: rsync / two-stage / script /
  docker / git / http output and progress are written to the run log in
  real time
- **Verbose per-file rsync output** in the run log (human-readable
  sizes, matching the plain rsync provider)

## Fixes

- TUI: opening the console no longer accumulates blank lines in the
  network sections of the config (idempotent writes); the first load no
  longer freezes (the background refresh no longer holds the snapshot
  lock across network calls); the F5 logs page renders again (the client
  parsed the plain-text logs endpoint as JSON and always failed)
- Dispatch queue: a job with an active run is no longer offered again
  (a queued sibling could block everything behind it); run history is
  ordered by creation time
- Provider child processes are reaped even when a run future ends
  abnormally (timeout, restart) — no more orphaned sync processes
- Migration script: git-class mirrors keep their own name in the storage
  path (matching their datasets); two-stage jobs map correctly
- API tokens are validated at config load (at least 32 bytes, no
  placeholders, no duplicates); migrations are embedded in the binary so
  systemd services no longer depend on the working directory
- CI workflow runs with a read-only GITHUB_TOKEN
