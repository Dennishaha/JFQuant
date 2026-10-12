# VS Code 调试入口

本目录提供可直接从“运行和调试”面板使用的任务和启动配置。先安装 Rust Analyzer 与 CodeLLDB 扩展；`factor_pipeline` 还需要本机可用的 LightGBM 动态库。

## 首次打开与扩展提示

工作区通过 [extensions.json](extensions.json) 推荐 `rust-lang.rust-analyzer` 和 `vadimcn.vscode-lldb`，并在 [settings.json](settings.json) 中允许扩展推荐提示。打开项目后可以点击提示安装，也可以在扩展面板搜索 `@recommended`。

如果出现“配置的类型 lldb 不受支持”，表示当前窗口没有启用 CodeLLDB。点击“安装 lldb 扩展”，确认安装的是 **CodeLLDB（vadimcn.vscode-lldb）**。LLVM 的 **LLDB DAP（llvm-vs-code-extensions.lldb-dap）** 提供的是另一种调试类型，不能直接支持本项目的 `type: lldb`。安装完成后重新选择调试配置；如果窗口仍未识别，执行 `Developer: Reload Window`。

也可以使用 CLI 安装：

```bash
code --install-extension vadimcn.vscode-lldb
code --install-extension rust-lang.rust-analyzer
```

macOS 未配置 `code` 命令时，可在命令面板执行 `Shell Command: Install 'code' command in PATH`，或使用 `/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code`。

## 常用入口

- `Rust: workspace tests`：运行整个 workspace 测试。
- `Rust: workspace clippy`：运行 `clippy` 严格检查。
- `Rust: native smoke`：使用官方 LightGBM C API 运行 FFI 训练/预测 smoke test。
- `Rust: factor pipeline`：从本地行情和历史股票池 Parquet 运行真实闭环。
- `Rust: desktop app`：启动 egui 应用。

不需要断点时，从“终端 → 运行任务”选择 `Rust: run native smoke (terminal)`、`Rust: run factor pipeline (terminal)` 或 `Rust: run desktop app (terminal)`。这些入口不需要 CodeLLDB。

## 环境与路径

`LIGHTGBM_LIB_DIR` 与 `DYLD_LIBRARY_PATH` 直接从 VS Code 进程环境读取，避免通过自定义设置嵌套变量（VS Code 变量替换不会递归展开）。先设置环境，再从该终端打开 VS Code；已经运行的 VS Code 需要完全退出后重新打开：

```bash
export LIGHTGBM_LIB_DIR=/absolute/path/to/lightgbm/lib
export DYLD_LIBRARY_PATH="$LIGHTGBM_LIB_DIR:${DYLD_LIBRARY_PATH:-}"
code .
```

如果 LightGBM 依赖单独的 OpenMP 动态库目录，也将其加入 `DYLD_LIBRARY_PATH`。`jfquant.duckdbDownloadLib` 默认 `1`，控制 DuckDB 开发构建下载动态库。

行情、股票池和输出路径通过 `.vscode/settings.json` 的 `jfquant.marketPath`、`jfquant.universePath`、`jfquant.outputPath` 配置。支持相对于 workspace 根目录的路径或绝对路径；不要在这些值里嵌套 `${workspaceFolder}`。日期和因子参数分别在 `launch.json`、`tasks.json` 中修改。

## 验收

- 新窗口打开 workspace，在扩展面板 `@recommended` 中可见 Rust Analyzer 与 CodeLLDB。
- 启用 CodeLLDB 后，选择 `Rust: desktop app` 并按 F5，可在 Rust 源码断点处暂停。
- 无 CodeLLDB 时，`Rust: run desktop app (terminal)` 仍可构建并启动应用。
- 配置真实行情路径与 native library 环境后，分别运行 native smoke、factor pipeline，检查终端结果与 `.jfquant/runs/` 下产物。生成产物不得提交。

相关机制见 [VS Code 扩展推荐文档](https://code.visualstudio.com/docs/configure/extensions/extension-marketplace) 与 [CodeLLDB 官方说明](https://github.com/vadimcn/codelldb)。

真实闭环所需的数据字段和命令参数见 [ml README](../crates/ml/README.md) 与 [engine README](../crates/engine/README.md)。
