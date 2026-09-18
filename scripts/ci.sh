#!/usr/bin/env bash
# Run the same checks locally and in GitHub Actions. PostgreSQL tests require
# SYNORA_TEST_PG_URL pointing to a disposable, empty database.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
python3 - <<'PYTHON'
import pathlib
import tomllib

version = pathlib.Path("VERSION").read_text().strip()
manifest = tomllib.loads(pathlib.Path("Cargo.toml").read_text())
lock = tomllib.loads(pathlib.Path("Cargo.lock").read_text())
assert manifest["workspace"]["package"]["version"] == version, "VERSION differs from Cargo.toml"
for package in lock["package"]:
    if "source" not in package:
        assert package["version"] == version, f"VERSION differs from {package['name']} in Cargo.lock"
PYTHON
cargo clippy --workspace --all-targets --locked -- -D warnings
if [[ -n "${SYNORA_TEST_PG_URL:-}" ]]; then
    cargo test --workspace --locked -- --include-ignored
else
    cargo test --workspace --locked
    echo "PostgreSQL integration test skipped locally: set SYNORA_TEST_PG_URL to an empty disposable database."
fi
cargo build --workspace --locked
