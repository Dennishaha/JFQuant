# 年度滚动研究内部模块

[上级：ML源码](../README.md) · [公共请求与结果](../../README.md#中证500年度滚动研究) · [股份权益](../../../core/src/README.md)

本目录是 `backtest.rs` 的内部实现，不提供独立CLI或旧口径。公开入口由父模块 `ResearchRequest/run_with_context` 提供，App/CLI均通过workflow适配器调用。

## 模块与输入输出

| 文件 | 职责 |
|---|---|
| [data.rs](data.rs) | 756交易日预热、历史成员面板、因子列、跨边界标签；年度 `.json + .bin` 缓存和矩阵拼接 |
| [raw_portfolio.rs](raw_portfolio.rs) | 公司行动准入、每日原价与常规限价缓冲、调仓、T+1；`RawPortfolio` 跨模型年份持续 |
| [portfolio.rs](portfolio.rs) | `DayResult/Performance`、每日IC/RankIC与收益/回撤/费用统计；没有复权碎股模拟器 |
| [report.rs](report.rs) | 先校验schema13严格结果，再渲染report.md和nav.csv；不训练或改变策略 |

```mermaid
flowchart TD
  INPUT["冻结行情、公司行动、模板与公式版本"] --> READY["训练前：价量、标签、历史调整依据"]
  READY --> BLOCK["失败：execution_readiness，停止训练"]
  READY --> FACTOR["data.rs：复权因子视图、成员掩码与预热"]
  FACTOR --> CACHE["年度有序 float32 矩阵与标签"]
  CACHE --> MODEL["每年训练/验证、purge 边界、样本外预测"]
  MODEL --> RAW["raw_portfolio：日终信号，下一开盘原价成交"]
  RAW --> ACCOUNT["同一 ShareLedger、估值、应收、调仓时钟"]
  ACCOUNT --> NEXT["下一年度模型：保留账户状态"]
  NEXT --> RAW
  ACCOUNT --> STATS["portfolio：年度与连续账户统计"]
  STATS --> REPORT["report：schema13、年度表与净值 CSV"]
```

## 时间、缓存和撮合语义

训练、验证和样本外使用所选Schedule的真实日期，5/1仅为内置模板默认；首折仍受历史覆盖约束。因子输入可复权，成交和估值通过行情/factor恢复原价，不能以复权价格计算真实股数。

缓存 `YearMeta` 保存signature、factor_ids、Sample行及finite_counts；`.bin`为row-major little-endian f32，NaN是missing。加载核对签名、列顺序与字节形状，旧近似报价/基准字段不属于当前缓存。没有当前数据版本内容签名时先重新登记。

账户逐日先推进到账及可售状态，执行前一日信号，收盘登记权益并计算包含应收的NAV。停牌不成交，除权日调整持仓标记避免重复收益；最后一日信号可以交给下一年度。模型换代不重置现金、股数或调仓时钟，末年也不构造虚拟强制卖出。

缺实际支付/上市日、源冲突、未知复权因子跳变、配股、零碎股分配或重叠未上市动作解释失败时停止。常规板块限制仍使用0.5个百分点保守缓冲；缺少ST/上市特例、精确限价、逐笔流动性与退市清算，不能宣称完整实盘复现。

## 结果、手算与验收

完整产物表见 [ML README](../../README.md)。schema13包含双签名、公式版本、执行配置、年度报告及汇总；`validate_result`被报告、平台读取和只读导入复用。旧格式返回unsupported_result_format，不迁移或重新计费。

合成平台fixture用1005元、最低5元佣金买100股；每股分红0.1元并送0.1股，之后原价9元，两个样本外年末均为110股与现金10元，累计费用5元，总收益−5/1005。跨年延迟支付/上市fixture另外验证登记后出售、到账前不可花费/出售和除权估值。

从仓库根目录执行，原生环境见 [Crates导航](../../../README.md#开发与验收)：

```bash
cargo nextest run -p analytics-ml --features lightgbm-native,duckdb-native -E 'test(backtest::)'
cargo nextest run -p analytics-workflow -E 'test(synthetic_annual) | test(incomplete_action)'
cargo nextest run -p analytics-desktop -E 'test(research_worker) | test(unsupported_history)'
```

真实公开数据仍可能被严格准入阻止；成功的合成研究只验证流程和账本，不是中证500历史收益结论。

## 统一行情与动态资格

年度流程通过storage `DailyDataset`读取日历、因子字段和独立原价，Qlib与规范Parquet共用模型和账本。因子价量保持同一重基准规则，输入变化纳入缓存签名。基准从模板显式传入，同时用于因子与收益比较。

```mermaid
flowchart TD
  INPUT["Qlib / canonical Parquet"] --> DAILY["DailyDataset因子字段与raw_field"]
  POOL["冻结的逐日股票池"] --> SAMPLE["信号日因子截面与训练样本"]
  DAILY --> SAMPLE
  SAMPLE --> PREDICT["年度模型产生次日信号"]
  PREDICT --> CHECK["执行日再次检查资格"]
  POOL --> CHECK
  CHECK --> LEDGER["整数股、T+1、跨年持仓与权益"]
  POOL --> EXIT["移除后按策略启动退出，阻碍持续重试"]
  EXIT --> LEDGER
  LEDGER --> EVENTS["pool_events.jsonl、年度账本、结果schema13"]
```

移除持仓的默认策略为下一可交易日退出，可选择下次正常调仓启动退出；该退出不依赖新的预测。停牌、涨跌停及T+1阻碍会记录并重试；重新入池取消未成交退出，正常调仓避免重复卖出。退池前登记的分红和红股权益继续结算，红股上市后仍按股票池资格处理。空池保持现金和既有权益，不扩大股票池。训练/验证无有效样本失败，训练价量依据缺失时停止训练；实际持仓权益缺失时停止回测。

`execution_readiness.json` 记录 `factor_basis_issues / adjustment_issues / affected_factor_ids`；研究价量来自独立系数，历史核验覆盖训练、验证、预热和测试。缺事件、缺跳变、未知参考价、逆向系数或特殊行动均在缓存和训练前停止。

系数来源、锚点及规则冻结到execution和每fold模型manifest，并加入factor/research签名。raw_portfolio::adjustment_issues核验训练、验证、预热及测试历史；validate_actions另要求账户支付/上市日期。RawFields读取raw_volume和独立adjustment_factors，撮合不使用研究成交量。

## 日期窗口与完整事件

当前窗口由公共Schedule/Fold提供，已删除training_years、validation_years及首末测试年份参数。固定5+1只是内置rolling模板；首折可自由指定训练、可选验证和测试日期，单次/手动模式不按自然年裁切。年文件是缓存分片，不是训练边界。test标签越界不参与IC，尾部预测保留。

公司行动参考调用CorporateAction::reference_price，支持差异化分红、配股参考及公告明确的无除权事件；每个实际参考变化仍需事件解释。执行准备检查后继证券原价或日期化待交付估值。各fold的daily只含本折测试日，gap_daily记录前面的空档；总daily和自然年报告覆盖完整账户。

## 为什么需要本组件

将模型折与账户时间分开。模型可以按任意日期滚动，现金、持仓、权益和调仓时钟不能随折重置。测试间隔继续估值和结算，证券承继转移真实权益而不拼接价格；年度结果由连续账户汇总。

设计依据：2026-10-14接受的当前组件职责、可复现实验与用户研究需求、组件接口及测试。未在代码或证据中确认的历史取舍不推定为既有设计理由；来源、法规或数据格式变化时重新评估。



## 三级研究模式：为什么允许筛选

决定日期2026-10-14，依据是用户接受的三级模式计划。正常预热空值、来源缺失和错误数据是不同问题：全部拒绝会浪费可用样本，全部放行又会混淆因子和真实权益。默认严格检查；另外提供“允许部分数据缺失”和“只用数据合格的股票”。模式是研究输入的一部分，不能作为训练失败后自动切换的兜底。

第二档保持原成员，纯缺失传播null；第三档派生每日合格成员，不改原始成员或证据。不能看到后来缺价而倒删早期股票。账户仍采用原价、整数股、T+1和连续权益；未裁定冲突、损坏、基准缺项及剩余必要依赖仍阻断。默认有效因子比例0表示至少一项有效，最低有标签样本1只表示可执行，不表示样本在统计上充分。所选因子整列无有效值不会被自动删除。

[quality.rs](quality.rs)保存原池和派生池，资格只在对应日期起生效；候选资格仅由必要价量输入生成，事件类别覆盖不参与每日训练筛选。来源失败未能区分缺失与真实冲突时保守阻断，不依据字符串放行。实际持仓的执行缺口必须停止账户计算，不能靠删股绕过。

[data.rs](data.rs)的同一selection_mask用于训练、验证和预测；前两者要求截止前的标签，预测只检查有效因子。最低比例默认0但至少一项有效，严格模式保持现有因子null规则。report.rs显示中文模式和研究范围；quality.json、admission.json、sample_selection.json与sample_masks共同提供离线可审计记录。验收包括未来行情/事件缺口不修改早期成员、损坏/冲突阻断和源格式一致性。

```mermaid
flowchart LR
  P[模式与门槛] --> C[结构化数据检查]
  C --> U[原股票池或每日合格池]
  U --> F[因子与nullable输入]
  F --> M[按折样本掩码]
  M --> G[训练剩余依赖门禁]
  G -->|通过| T[冻结签名后训练]
  G -->|未通过| E[原因及处理清单]
```

## 2026-10-14：直接代码读取与分阶段研究

为什么调整：普通市场代码无需先登记身份；独立目录、审核和权威名录下载增加了研究前置负担。特殊历史代码仍需在快照内明确区间，不能将旧证券行情或权益接到另一证券。训练使用价量和标签，到账／交付资料只在实际账户执行时检查。

当前格式：模板9、需求3、准入5、股票池3、公司行动5、规范行情5、系数3、采集7、服务11、研究结果13；快照内来源映射1、训练产物2。工作区和任务记录1、GTJA191公式v4及默认178项顺序保持。旧格式明确拒绝，不自动转换。

```mermaid
flowchart LR
  D[数据快照与少量来源例外] --> M[批量价量掩码与标签边界]
  M --> T[训练与独立模型产物]
  T --> P[测试期预测]
  P --> E[实际订单和持仓检查]
  E --> R[完整结果或未完成原因]
```

验收：nextest覆盖来源日期边界、三档掩码、训练／回测分离及模型保留；Clippy、Rustdoc、格式、组件文档和包内CLI共同检查。生成数据、模型、运行记录和安装包不提交。另一台Apple Silicon/macOS14实机验收继续待完成。

## 为什么将重复诊断合并为区间

真实Qlib快照可能有连续的未解释系数变化；按交易日重复输出会掩盖证券级原因并使服务JSON超出读取限制。检查仍逐观察值对账，但相同原因的相邻交易日仅展示起止区间，不同原因保持分开。admission只摘要计数与首批原因；完整区间留在execution_readiness.json。阈值保持原值，压缩不是放宽准入。

训练签名冻结最终样本掩码、折日程、模型参数、历史调整依据与特征顺序。回测execution.training_provenance引用稳定训练签名、有序特征及模型字节哈希；训练描述和manifest的完整哈希另由training.json及任务快照校验，随机model_id不会改变同一研究的语义签名。

## 账户门禁仍独立

provider只改变历史训练对账的处理方式，raw_portfolio.rs不改原价、整数股、权益和估值要求；实际账户失败保留已训练模型。不同训练组合使用同一真实账户规则，不能借对照实验换成本或资金。 完整接口、why、边界和验收见[组件契约](../../README.md)。

YearInputs传入实际universe_id，因子内部请求不再写死中证500；池、基准和签名始终来自解析后的TargetProfile。这个调整避免跨指数研究留下误导性元数据，不改变GTJA公式或Alpha算子。

## 为什么使用执行索引

原实现每笔买入调用全包扫描并要求九类证明。RawPortfolio现在持有一次构建的ExecutionIndex，按当日公开知识与实际账户依赖检查；保留供应商覆盖告警，记录稳定拒单原因。独立训练产物2保持不变，backtest_from_training_with_policy仅改变执行策略，模型与特征口径必须匹配原产物。当天无法解释的系数变化仍拦截，不放宽事件对账0.002阈值。

`AdjustmentContext`把账户日期、执行索引和行情系数类型传给逐日检查。优先选冻结动作包内的独立新浪系数，按最近已生效点计算；缺历史锚点或超过验证截止日明确报错。恢复原价的Qlib字段不再被误用为该独立证据。回归覆盖Qlib字段有微动但独立系数不变，以及真实独立跳变仍被拦截。
