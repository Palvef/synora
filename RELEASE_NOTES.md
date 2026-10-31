# Synora 0.2.5

- Separate temporary worker shutdown (`STOPPING`) from operator retirement (`DRAINING`). Registration after a restart restores temporarily stopped workers to `ONLINE`, including when shutdown was interrupted. Explicit retirement and maintenance remain durable. Assignment leases and fencing checks remain enforced.
- Read PyPI cache votes from a dedicated read-only ClickHouse view instead of scanning Nginx log files. Validate complete, fresh seven-day results before cache eviction; preserve subsecond time boundaries and report sanitized validation errors.
- Detect same-sized rapid configuration edits even when filesystem timestamps are unchanged, while reusing parsed documents for unchanged content.
- Add optional APT request pacing through `APT_REQUEST_INTERVAL` without changing retry policies, synchronization scope or integrity checks.
- Use HTTPS for VirtualBox and honor its configured upstream.

Linux binaries target Ubuntu 24.04 / glibc 2.39 or newer; Ubuntu 22.04 is no longer supported. CLI, Manager, and Worker archives are available for x86_64 and ARM64. Synchronization images are built and published separately through Actions to GHCR.
