# VS Code 调试入口

本目录提供可直接从“运行和调试”面板使用的任务和启动配置。先安装 Rust Analyzer 与 CodeLLDB 扩展；`factor_pipeline` 还需要本机可用的 LightGBM 动态库。

## 常用入口

- `Rust: workspace tests`：运行整个 workspace 测试。
- `Rust: workspace clippy`：运行 `clippy` 严格检查。
- `Rust: native smoke`：使用官方 LightGBM C API 运行 FFI 训练/预测 smoke test。
- `Rust: factor pipeline`：从本地行情和历史股票池 Parquet 运行真实闭环。
- `Rust: desktop app`：启动 egui 应用。

`LIGHTGBM_LIB_DIR` 和 `DUCKDB_DOWNLOAD_LIB` 从 VS Code 进程环境读取。若使用固定路径，可在 `.vscode/settings.json` 的 `jfquant.*` 设置中配置；这些设置不会写入模型、数据集或 native library。

真实闭环所需的数据字段和命令参数见 [ml README](../crates/ml/README.md) 与 [engine README](../crates/engine/README.md)。
