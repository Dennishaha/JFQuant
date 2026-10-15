# analytics-desktop

macOS优先的egui桌面客户端，提供数据、查询、因子、建模、研究和运行记录界面。
所有计算通过同一工作区服务，App不持有DuckDB、LightGBM或Python对象。

```mermaid
flowchart LR
 UI[egui面板] --> C[服务客户端与事件适配]
 C --> W[workflow：任务与持久化]
 W --> R[Rust因子 / Python LightGBM / AKQuant]
 R --> O[结果14与运行日志]
 O --> UI
```

公共边界为`WorkerCommand`、`WorkerEvent`及workflow中的请求类型。日期、因子和标的
配置独立编辑；任务开始时冻结，关闭界面不终止后台。界面支持进度、取消、重试、
仅训练和使用已有模型回测，展示训练产物与回测停止原因。

当前服务14、工作流10、训练描述3、模型manifest2、结果14。旧格式不自动转换，
状态和日志仍可查看。研究页展示AKQuant事件日口径及执行费用合计，不展示已删除的
精细权益账本，不提供不支持的净佣、配股认购或个人分红扣税控件。

```bash
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo build -p analytics-desktop -p analytics-cli
DUCKDB_DOWNLOAD_LIB=1 cargo run -p analytics-desktop
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-desktop
```

验收包括真实egui交互、App与CLI共用任务、取消、错误、配置保存与旧格式拒绝。
测试使用包内研究运行时，无Rust LightGBM链接或Homebrew运行依赖。
[打包说明](packaging/README.md)描述自包含Apple Silicon/macOS14构建；另一台干净实机
安装验收仍需实际执行。[后端口径](../workflow/research/README.md)列出明确能力边界。
