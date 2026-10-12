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

打开 workspace 后可按扩展推荐提示安装 Rust Analyzer 和 CodeLLDB；`lldb` 调试类型由 CodeLLDB 提供。随后从“运行和调试”面板使用 `native smoke`、`factor pipeline` 和 `desktop app` 配置。不安装调试扩展也可从“终端 → 运行任务”选择对应的 `(terminal)` 入口。数据路径和输出目录集中在 `.vscode/settings.json`，LightGBM 环境从 VS Code 进程读取；环境配置与验收步骤见 [.vscode/README.md](.vscode/README.md)。

