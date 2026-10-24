# Synora 0.2.4

- Keep task state, live resource gauges, and worker slot usage consistent with effective run assignments. Late worker reports cannot restore finished task resources.
- Batch task-list database reads and keep TUI navigation responsive during slow requests and background operations.
- Cache parsed configuration files and apply only added, changed, or removed jobs on reload. Reload reports the actual change count and does not trigger catch-up runs.
- Show current resources separately from previous completion data in the synora-mirror Grafana dashboard, with a five-second refresh and the existing fifteen-second collection interval.
- Preserve the PyPI access-log validation cache and ignore unusable request paths without ignoring corrupted logs.
- Retain published Nix channels and report warnings when the official cache has removed objects required by a new release.

Linux binaries target Ubuntu 24.04 / glibc 2.39 or newer; Ubuntu 22.04 is no longer supported. CLI, Manager, and Worker archives are available for x86_64 and ARM64. Synchronization images are built and published separately through Actions to GHCR.
