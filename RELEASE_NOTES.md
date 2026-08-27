# Synora 0.1.9

## Improvements

- Native HTTP mirrors now plan directory trees concurrently, expose live
  planning and per-file transfer progress, update byte counters per chunk,
  and support root-relative path-prefix exclusions
- Distributed worker log tails are forwarded with heartbeats so the manager
  and TUI can display active HTTP transfers before a run finishes
- Worker shutdown and rolling upgrades now drain in-flight jobs instead of
  cancelling them; only an explicit operator stop records `Cancelled`
- Job `timeout` is an optional manager-supplied hard limit. Omitting it waits
  for natural completion, while an exceeded limit is reported as a failure

## Fixes

- Script and container providers can no longer report success when their
  process exits unsuccessfully
- Prometheus CPU, memory, CPU-percent, and bandwidth gauges now contain only
  active jobs and are removed when a task finishes; cumulative CPU history
  remains available through `synora_job_cpu_usage_seconds_total`
- HTTP planning reacts to cancellation immediately and no longer serializes
  large directory listings
