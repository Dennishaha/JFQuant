# analytics-desktop

Local-first Parquet analysis, Alpha191 factor computation, and LightGBM training through an official C API FFI.

The workspace is split into `core`, `storage`, `engine`, `factor`, `ml`, and `app`. The UI is macOS-first and all long-running work runs off the egui thread.

```mermaid
flowchart LR
  P[Local Parquet] --> S[storage]
  S --> E[DuckDB engine]
  E --> F[Alpha191 factor executor]
  F --> C[Factor cache Parquet]
  C --> M[LightGBM FFI trainer]
  M --> O[Model + manifest + predictions]
```

Run the dependency-free checks with:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo check -p analytics-engine --features duckdb-native
cargo check -p analytics-ml --features lightgbm-native
```

Native DuckDB and LightGBM builds are optional features documented in the component READMEs.
The `duckdb-bundled` feature requires a local DuckDB source checkout for the current upstream crate version.

To run the native factor/training/prediction path against local market and historical-universe Parquet files:

```bash
LIGHTGBM_LIB_DIR=/path/to/lightgbm/lib \
DYLD_LIBRARY_PATH=/path/to/lightgbm/lib:/path/to/openmp/lib \
DUCKDB_DOWNLOAD_LIB=1 \
cargo run -p analytics-ml --features lightgbm-native,duckdb-native --example factor_pipeline -- \
  --market /path/to/market --universe /path/to/universe \
  --start 2020-01-01 --end 2025-12-31 --output /tmp/jfquant-factor-pipeline
```

The command writes `factors.parquet`, `factor_run_manifest.json`, `model.txt`, `manifest.json`, `metrics.json`, `feature_importance.json`, and `predictions.parquet` outside the repository.

## VS Code 调试

安装 Rust Analyzer 和 CodeLLDB 后，打开“运行和调试”面板即可使用 `.vscode/launch.json` 中的 `native smoke`、`factor pipeline` 和 `desktop app` 配置。路径、输出目录和 LightGBM 环境变量集中在 `.vscode/settings.json`；对应的测试、构建和 Clippy 任务见 `.vscode/tasks.json`。详细说明见 [.vscode/README.md](.vscode/README.md)。

