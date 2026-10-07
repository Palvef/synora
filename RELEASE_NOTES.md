# Synora 0.2.3

- Build and publish all seven synchronization runtimes from source through GitHub Actions to GHCR.
- Stream PyPI synchronization logs while the task runs, with periodic access-log, cache-scan, and download progress.
- Resume interrupted GitHub release downloads with bounded retries and size/digest validation. Keep one latest release per configured project and use the official Latest release for stable projects.
- Preserve signed AOSP upstream branch tips, including branches that point to tags.

Linux release archives include the CLI, Manager, and Worker. Builds verify Ubuntu 22.04 / glibc 2.35 compatibility.
Production-specific deployment configuration remains outside this release.
