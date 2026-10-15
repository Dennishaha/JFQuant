# analytics-cli / jfquant

独立clap命令行客户端，和App共用同一工作区服务。stdout只输出JSON，长任务立即返回
`task_id`；jobs命令用于等待、读取状态、日志、结果和取消。无需打开GUI或运行Codex。

```mermaid
flowchart LR
 CLI[jfquant命令与JSON] --> C[workflow Client]
 C --> S[Unix socket工作区服务14]
 S --> W[数据 / Rust因子 / Python研究worker]
 W --> R[记录与结果14]
 R --> CLI
```

```bash
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo build -p analytics-cli
jfquant workspace create "$HOME/JFQuantWorkspace"
export JFQUANT_WORKSPACE="$HOME/JFQuantWorkspace"
jfquant research demo
jfquant jobs wait TASK_UUID
jfquant jobs results TASK_UUID
jfquant templates default --output recipe.json
jfquant research check --template recipe.json --market MARKET_VERSION --actions ACTION_VERSION
jfquant research train --template recipe.json --market MARKET_VERSION --actions ACTION_VERSION
jfquant research backtest --training TRAINING_TASK_UUID --actions ACTION_VERSION
jfquant query run --sql 'SELECT 42 AS answer'
```

`research run`执行完整研究；`train`和`backtest`分别运行独立阶段。因子、标的与工作流
仍可独立保存和引用，参数格式以`--help`及内置模板为准。`run --request`接受与App相同
的公共请求，不执行任意Python脚本。

研究使用包内LightGBM和AKQuant，不再链接Rust LightGBM。分红、拆股采用事件日口径，
仅报告执行费用合计；不支持事件或映射失败保留模型并返回任务诊断，不发布完整收益。
无有效交易与数据阻断分别展示。详细能力边界见[研究worker](../workflow/research/README.md)。

退出码：成功0，参数错误2，数据阻断3，忙碌4，等待超时124，取消130，其他失败1。
关闭客户端不终止后台，取消由同一服务执行。安装包CLI位于
`/Applications/JFQuant.app/Contents/MacOS/jfquant`，无需Cargo、外部Python或Homebrew。

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-cli
DUCKDB_DOWNLOAD_LIB=1 cargo clippy -p analytics-cli --all-targets -- -D warnings
```

验收覆盖CLI与App公共结果、训练复用、格式隔离、取消、服务恢复及离线读取。
生成数据、模型、日志、凭据及安装包不提交。命令定义和接口目录见[src/README.md](src/README.md)。

`research backtest --training <ID> --actions <ID> --template <PATH>` 可复用模型并更改账户参数。
可选模板仅覆盖资金、费用、持仓数、调仓周期和退池策略，不改变模型或训练窗口。
