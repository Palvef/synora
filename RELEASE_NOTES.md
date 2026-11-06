# Synora 0.2.6

- Fix authenticated HTTP proxy challenges so Git reconnects correctly after a 407 response instead of reporting `Proxy CONNECT aborted`.
- Report PyPI synchronization User-Agent (`Synora-pypi-yukina`) and bounded upstream HTTP rejection diagnostics, including TUNA denial codes and request identifiers.
- Add optional on-demand PyPI index, JSON and package metadata caching with separate HTML/JSON cache keys and no caching of upstream errors.
- Update the synchronization image's AOSP repo launcher to the current stable revision (launcher 2.71).

Linux binaries require Ubuntu 24.04 / glibc 2.39 or newer. Synchronization images are published separately through Actions to GHCR.
