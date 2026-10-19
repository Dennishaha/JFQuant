# Component navigation

| Component | Responsibility |
|---|---|
| [core](core/README.md) | Serializable research, query, manifest and data contracts |
| [storage](storage/README.md) | Acquisition, provenance, immutable snapshots and formats |
| [engine](engine/README.md) | Worker-local DuckDB queries and exports |
| [factor](factor/README.md) | GTJA191 / Alpha158 definitions, execution and reference checks |
| [workflow](workflow/README.md) | Workspace service, tasks, configuration and research orchestration |
| [app](app/README.md) | Desktop client and product interactions |
| [cli](cli/README.md) | Standalone command-line JSON interface |

Research uses the unchanged Rust factor engine, official Python LightGBM, and AKQuant
continuous backtests through a supervised Python process. The former ml crate, native
LightGBM FFI and custom account simulator have been removed. No PyO3 or upstream fork
is required. See [research runtime](workflow/research/README.md).

```bash
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run --workspace
DUCKDB_DOWNLOAD_LIB=1 cargo clippy --workspace --all-targets -- -D warnings
```

Generated data, dependencies, models and distribution artifacts are never committed.

The current desktop/CLI service contract is 43; collector protocol is 16 and research worker
protocol is 4. GTJA191 v5 exposes 191 implementations with a 190-factor default; Alpha030
requires real dated three-factor inputs. BigQuant acquisition and offline factor publication
belong to storage/workflow. See the component READMEs for formats, coverage and acceptance.
