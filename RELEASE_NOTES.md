# Synora 0.2.2

## Repository synchronization

- Discover APT/RPM distribution versions, components, and architectures from
  upstream metadata. Job configuration can override APT and RPM architectures
  independently, including different architecture sets for individual URL paths.
- Filter debug-symbol and test packages by default. Retain shared APT packages
  when unselected suites still reference them, and regenerate filtered RPM indexes.
- Repair MongoDB RPM repositories whose primary metadata is missing, using a
  validated upstream object inventory. Process RPM repositories independently
  and recover interrupted metadata generation under a repository lock.
- Use current InfluxData endpoints and discover XanMod's published suites.
  LLVM and Elastic versions continue to be discovered automatically.
- Add mirror scripts for PyBOMBS and improve Nix channel retention and cleanup.
  Protect Nix's latest alias and defer deletion until downloads succeed.

## HTTP and task execution

- Support glob exclusions for HTTP mirrors, including unwanted architecture
  directories, prereleases, and auxiliary files.
- Allow ordinary HTTP 403 files to be reported as warnings when explicitly
  configured. Missing critical metadata, incomplete traversal, and transfer or
  integrity failures remain failures. Incomplete runs suppress deletion.
- Deduplicate listing destinations before concurrent downloads.
- Push-triggered restarts wait for cancellation to be acknowledged before
  starting a replacement run.

## Runtime images and documentation

- Split synchronization runtimes into synora-scripts, synora-rubygems,
  synora-rustup, synora-nix-channels, synora-yukina, synora-shadowmire, and
  synora-ftpsync. Build them with scripts/build-sync-images.sh.
- Keep architecture choices in job configuration instead of hardcoded wrapper
  scopes; unspecified selections retain automatic discovery.
- Document the current implementation and remove obsolete roadmap comments.
  Production-specific PyPI deployment files remain outside the repository.

## Release binaries

- Linux packages contain the CLI, Manager, and Worker as separate archives.
- Release builds check compatibility with Ubuntu 22.04 / glibc 2.35.
- Docker runtimes are built separately from these binary archives; rebuilding
  them is necessary to receive synchronization-script changes.
