#!/bin/bash
# Run from any directory. Runtime rebuilding is deliberately separate from tests.
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$root"
export DUCKDB_DOWNLOAD_LIB=1
python3 crates/workflow/checks/architecture.py
python3 -B -m unittest discover -s crates/workflow/checks -p 'test_execution_dependencies.py'
cargo fmt --all -- --check
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
PYTHONPATH=crates/workflow/research target/collector-build/python/bin/python3 -B -m unittest discover -s crates/workflow/research/tests
PYTHONPATH=crates/storage/collectors target/collector-build/python/bin/python3 -B -m unittest discover -s crates/storage/collectors/tests
