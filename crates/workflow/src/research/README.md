# Research orchestration

Rust owns ordered feature contracts, nullable matrices, source/data qualification,
chronological folds, label purging, IC and result metadata. `pipeline` handles the
single-run factor/model flow; `backtest` handles dated rolling research and a single
continuous AKQuant portfolio. `worker` supervises the fixed Python operations.

`LightGbmTrainer` and `LightGbmPredictor` are file/protocol adapters, not C FFI wrappers.
They write named Arrow-null-preserving Parquet matrices, reuse the task's process,
validate model manifests and record dependency receipts. The Python process keeps
all native models. `worker::TaskSession` retires it after completion or unwind.

No Rust account ledger, matcher or LightGBM native build remains. Factor formulas
and reference tests remain in analytics-factor. Trading limitations and protocol
are defined in [../../research/README.md](../../research/README.md).

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-workflow research::
DUCKDB_DOWNLOAD_LIB=1 cargo clippy -p analytics-workflow --all-targets -- -D warnings
```
