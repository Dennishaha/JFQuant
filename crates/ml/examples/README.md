# ML 开发例子与离线绘图

[上级：analytics-ml](../README.md) · [模型实现](../src/README.md) · [App分发自检](../../app/packaging/README.md)

本目录只保留原生C API开发验收和已完成研究的可选绘图工具。业务调用使用独立`jfquant factors/model/research`，不提供factor_pipeline或rolling_backtest旧命令。

## 文件与验证对象

[native_smoke.rs](native_smoke.rs) 创建小矩阵，训练、读取manifest并执行普通预测/贡献值，检查官方LightGBM接口与产物形状。[plot_research.py](plot_research.py) 读取schema11严格研究的summary.json和nav.csv，以初始净值1计算回撤并生成nav.png/nav.svg；需要开发用matplotlib，不参与训练、策略或安装包运行。

```mermaid
flowchart LR
  SMALL["开发用小训练矩阵"] --> SMOKE["native_smoke：真实 C API"]
  SMOKE --> ART["模型、manifest、预测与贡献值检查"]
  RESEARCH["已完成的 schema11 原价研究"] --> JSON["summary.json 与 nav.csv"]
  JSON --> PLOT["plot_research.py：离线 matplotlib"]
  PLOT --> FIG["NAV、回撤 PNG 与 SVG"]
```

## 命令与输出边界

从仓库根目录执行；先按[Crates导航](../../README.md#开发与验收)设置原生环境，特别是直接运行example时的LightGBM/OpenMP库路径。

```bash
cargo run -p analytics-ml --features lightgbm-native --example native_smoke -- .jfquant/runs/native-smoke
python3 crates/ml/examples/plot_research.py .jfquant/runs/RESEARCH_TASK_UUID
```

绘图输入必须是完成的当前研究目录；旧schema或其他执行口径拒绝，不能修改schema数字绕过检查。标题的特征数量来自冻结research_config；窗口、账户和公式以execution/manifest为准，不能硬编码旧189项或年度重置说明。

生成模型与图片在`.jfquant/`，不提交Git。安装包原生自检通过`jfquant self-check --output DIR`调用公共后台；本example是开发检查，不能代替[清空环境包内验收](../../app/packaging/README.md#验收边界)。

```bash
cargo check -p analytics-ml --examples --features lightgbm-native
cargo nextest run -p analytics-ml --features lightgbm-native,duckdb-native
```

plot_research.py只接收schema11、raw_integer_shares及价量来源哈希；读取benchmark_price_index_nav列，标题取冻结benchmark，不固定中证500。此开发绘图工具需要matplotlib，不作为App运行依赖。

## 为什么需要本组件

提供当前研究结果的可复现查看方式。示例只读取当前结果格式和冻结来源，不重建旧近似收益。绘图展示折、自然年度、费用和净值，让报告能够离线审查；示例数据及图片属于本地产物。

设计依据：2026-10-14接受的当前组件职责、可复现实验与用户研究需求、组件接口及测试。未在代码或证据中确认的历史取舍不推定为既有设计理由；来源、法规或数据格式变化时重新评估。

## 为何绘图工具只接受结果11

plot_research.py只展示完成且经当前格式校验的研究；目录2与准入4改变了来源含义，因此旧结果不能直接换标签继续显示。身份更新任务不产生收益曲线。matplotlib仍仅为开发绘图依赖，不进入运行时业务。

## 2026-10-14：直接代码读取与分阶段研究

为什么调整：普通市场代码无需先登记身份；独立目录、审核和权威名录下载增加了研究前置负担。特殊历史代码仍需在快照内明确区间，不能将旧证券行情或权益接到另一证券。训练使用价量和标签，到账／交付资料只在实际账户执行时检查。

当前格式：模板6、需求3、准入4、股票池3、公司行动4、规范行情5、系数3、采集6、服务9、研究结果11；快照内来源映射1、训练产物1。工作区和任务记录1、GTJA191公式v4及默认178项顺序保持。旧格式明确拒绝，不自动转换。

```mermaid
flowchart LR
  D[数据快照与少量来源例外] --> M[批量价量掩码与标签边界]
  M --> T[训练与独立模型产物]
  T --> P[测试期预测]
  P --> E[实际订单和持仓检查]
  E --> R[完整结果或未完成原因]
```

验收：nextest覆盖来源日期边界、三档掩码、训练／回测分离及模型保留；Clippy、Rustdoc、格式、组件文档和包内CLI共同检查。生成数据、模型、运行记录和安装包不提交。另一台Apple Silicon/macOS14实机验收继续待完成。
