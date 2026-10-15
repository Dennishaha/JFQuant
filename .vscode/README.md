# VS Code development

Install Rust Analyzer and CodeLLDB, then build the shared packaged Python runtime:

```bash
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo build -p analytics-desktop -p analytics-cli
```

Launch targets are the desktop app and standalone jfquant CLI. Tasks provide builds,
workspace tests, strict Clippy and formatting checks. `jfquant.duckdbDownloadLib`
controls DuckDB's development download. `JFQUANT_COLLECTOR_RUNTIME` can point debug
builds to the packaged Python interpreter. No LightGBM linker environment or native
FFI smoke target remains; Python LightGBM and OpenMP are packaged together.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run --workspace
PYTHONPATH=crates/workflow/research target/collector-build/python/bin/python3 -B -m unittest discover -s crates/workflow/research/tests
```

See [research runtime](../crates/workflow/research/README.md) and
[desktop component](../crates/app/README.md). Generated outputs remain ignored.
