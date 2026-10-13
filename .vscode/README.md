# VS Code 调试入口

本目录提供可直接从“运行和调试”面板使用的任务和启动配置。先安装 Rust Analyzer 与 CodeLLDB 扩展；后台jfquant需要开发用LightGBM/DuckDB。

## 首次打开与扩展提示

工作区通过 [extensions.json](extensions.json) 推荐 `rust-lang.rust-analyzer` 和 `vadimcn.vscode-lldb`，并在 [settings.json](settings.json) 中允许扩展推荐提示。打开项目后可以点击提示安装，也可以在扩展面板搜索 `@recommended`。

如果出现“配置的类型 lldb 不受支持”，表示当前窗口没有启用 CodeLLDB。点击“安装 lldb 扩展”，确认安装的是 **CodeLLDB（vadimcn.vscode-lldb）**。LLVM 的 **LLDB DAP（llvm-vs-code-extensions.lldb-dap）** 提供的是另一种调试类型，不能直接支持本项目的 `type: lldb`。安装完成后重新选择调试配置；如果窗口仍未识别，执行 `Developer: Reload Window`。

当前工作区在 `settings.json` 中通过 `lldb.server` 指定 macOS Command Line Tools 的 `debugserver`，用于解决自动探测找不到调试服务器的问题。需要安装 Command Line Tools（`xcode-select --install`）；使用完整 Xcode、其他安装位置或其他平台时，将此项改为本机的 `debugserver` / `lldb-server` 路径，或删除该覆盖值以恢复 CodeLLDB 自动探测。

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
- `Rust: jfquant CLI`：构建并调试独立CLI，默认展示帮助；业务参数通过正式子命令或请求JSON配置。
- `Rust: desktop app`：同时构建analytics-desktop与analytics-cli，再启动egui应用；App自动调用同目录jfquant服务。

不需要断点时，从“终端 → 运行任务”选择 `Rust: run native smoke (terminal)` 或 `Rust: run desktop app (terminal)`。这些入口不需要 CodeLLDB。

## 环境与路径

`LIGHTGBM_LIB_DIR` 与 `DYLD_LIBRARY_PATH` 直接从 VS Code 进程环境读取，避免通过自定义设置嵌套变量（VS Code 变量替换不会递归展开）。先设置环境，再从该终端打开 VS Code；已经运行的 VS Code 需要完全退出后重新打开：

```bash
export LIGHTGBM_LIB_DIR=/absolute/path/to/lightgbm/lib
export DYLD_LIBRARY_PATH="$LIGHTGBM_LIB_DIR:${DYLD_LIBRARY_PATH:-}"
code .
```

如果 LightGBM 依赖单独的 OpenMP 动态库目录，也将其加入 `DYLD_LIBRARY_PATH`。`jfquant.duckdbDownloadLib` 默认 `1`，控制 DuckDB 开发构建下载动态库。

环境变量语义：

- `LIGHTGBM_LIB_DIR`：显式指定原生库目录时必须包含对应 LightGBM 动态库；为空或未设置时自动探测 `brew --prefix lightgbm`、标准 Homebrew 路径和当前用户的 Homebrew 路径。无法探测时才会链接失败。
- `DYLD_LIBRARY_PATH`：运行阶段查找 LightGBM 及 OpenMP 动态库的目录列表；VS Code 不会替用户递归展开嵌套变量。
- `DUCKDB_DOWNLOAD_LIB`：DuckDB native 开发构建的下载开关；VS Code 设置 `jfquant.duckdbDownloadLib` 默认传值 `1`，不影响系统 DuckDB 已安装的路径。
- `DUCKDB_LIB_DIR`：自定义 DuckDB 动态库目录，必须包含对应目标平台的库文件；使用下载模式时不要设置为空字符串，使用 `unset DUCKDB_LIB_DIR`。桌面 rpath 默认指向当前构建 profile 的 `deps`，不扫描其他架构或版本缓存。

行情、股票池、日期和因子配置保存在完整PipelineRequest JSON或App表单；CLI使用jfquant factors run --request FILE提交。路径从所选JFQuant工作区解析，运行产物由后台分配UUID。launch.json的jfquant配置可修改为正式子命令；不再使用旧example参数。

## 验收

- 新窗口打开 workspace，在扩展面板 `@recommended` 中可见 Rust Analyzer 与 CodeLLDB。
- 启用 CodeLLDB 后，选择 `Rust: desktop app` 并按 F5，可在 Rust 源码断点处暂停。
- 无 CodeLLDB 时，`Rust: run desktop app (terminal)` 仍可构建并启动应用。
- 配置真实行情路径与 native library 环境后，分别运行 native smoke 和 jfquant 标准因子流水线，检查终端结果与 `.jfquant/runs/` 下产物。生成产物不得提交。

相关机制见 [VS Code 扩展推荐文档](https://code.visualstudio.com/docs/configure/extensions/extension-marketplace) 与 [CodeLLDB 官方说明](https://github.com/vadimcn/codelldb)。

真实闭环所需的数据字段和命令参数见 [ml README](../crates/ml/README.md) 与 [engine README](../crates/engine/README.md)。

## 桌面交互自动化测试

运行任务 `Rust: desktop interaction tests (nextest)`，对应 `cargo nextest run -p analytics-desktop`。首次使用需要 `cargo install cargo-nextest --locked`。覆盖真实 egui 鼠标/键盘交互、状态校验与 DuckDB/LightGBM worker；详细验收范围见 [desktop README](../crates/app/README.md)。

## 查询时停在 DuckDB C++ 汇编

如果 CodeLLDB 在 `__cxa_throw` / `GlobFileList` 暂停，并勾选了“C++: on throw”，这是调试器在 DuckDB 内部异常抛出时暂停。DuckDB 会捕获异常并转换为查询错误，worker 将其显示在查询面板。

`Rust: desktop app` 的 `preRunCommands` 在启动前禁用 CodeLLDB 标记为 `cpp_exception` 的 C++ throw/catch 断点，让 DuckDB 完成内部捕获，随后在查询面板顶部显示错误。Rust 源码断点和 panic 断点仍然生效。该命令只影响本次桌面调试会话；如果需要调试 C++ 异常，可删除此命令并手动启用异常断点。命令运行时机见 [CodeLLDB 启动顺序](https://github.com/vadimcn/codelldb/blob/master/MANUAL.md#launch-sequence)。

修改启动配置后，停止当前会话，再选择 `Rust: desktop app` 按 F5 启动。当前已经暂停的旧会话可取消勾选“C++: on throw”和“C++: on catch”，按 F5 继续。

默认 SQL 读取 `data/market/**/*.parquet`；没有行情文件时，界面显示“未找到匹配的 Parquet 文件”以及原始 `No files found` 诊断。将 SQL 中的路径改为实际 Parquet 文件或目录，或使用 `SELECT 1 AS test` 验证。

## 后台 panic 与通道断开

桌面应用会捕获后台 Rust unwinding panic，并将原因显示在界面；普通 SQL/I/O 错误仍可直接重试。出现“后台不可用”时，点击顶部“重启后台”，再重新执行操作。重启创建新的线程和通道，不复用发生 panic 的原生句柄；之前已写出的文件不会自动回滚，因子/训练重试时可使用新的输出目录。

Rust panic 断点保留用于定位程序缺陷。在 VS Code 开启“Rust: on panic”时，调试器会先暂停，按 F5 继续后才能完成捕获并显示界面错误；需要直接验收恢复流程时，可使用 `Rust: run desktop app (terminal)` 或临时取消 Rust panic 断点。后台恢复的自动化故障注入用例只存在于测试构建，验收命令和边界见 [desktop README](../crates/app/README.md#后台异常与恢复)。
