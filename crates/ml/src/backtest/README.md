# 年度滚动研究内部模块

[上级：ML源码](../README.md) · [公共请求与结果](../../README.md#中证500年度滚动研究) · [股份权益](../../../core/src/README.md)

本目录是 `backtest.rs` 的内部实现，不提供独立CLI或旧口径。公开入口由父模块 `ResearchRequest/run_with_context` 提供，App/CLI均通过workflow适配器调用。

## 模块与输入输出

| 文件 | 职责 |
|---|---|
| [data.rs](data.rs) | 756交易日预热、历史成员面板、因子列、跨边界标签；年度 `.json + .bin` 缓存和矩阵拼接 |
| [raw_portfolio.rs](raw_portfolio.rs) | 公司行动准入、每日原价与常规限价缓冲、调仓、T+1；`RawPortfolio` 跨模型年份持续 |
| [portfolio.rs](portfolio.rs) | `DayResult/Performance`、每日IC/RankIC与收益/回撤/费用统计；没有复权碎股模拟器 |
| [report.rs](report.rs) | 先校验schema4严格结果，再渲染report.md和nav.csv；不训练或改变策略 |

```mermaid
flowchart TD
  INPUT["冻结行情、公司行动、模板与公式版本"] --> READY["训练前：日期覆盖、事件完整性与 factor 对账"]
  READY --> BLOCK["失败：execution_readiness，停止训练"]
  READY --> FACTOR["data.rs：复权因子视图、成员掩码与预热"]
  FACTOR --> CACHE["年度有序 float32 矩阵与标签"]
  CACHE --> MODEL["每年训练/验证、purge 边界、样本外预测"]
  MODEL --> RAW["raw_portfolio：日终信号，下一开盘原价成交"]
  RAW --> ACCOUNT["同一 ShareLedger、估值、应收、调仓时钟"]
  ACCOUNT --> NEXT["下一年度模型：保留账户状态"]
  NEXT --> RAW
  ACCOUNT --> STATS["portfolio：年度与连续账户统计"]
  STATS --> REPORT["report：schema4、年度表与净值 CSV"]
```

## 时间、缓存和撮合语义

训练使用测试年前 `training_years` 年，验证使用紧邻测试前 `validation_years` 年；默认5/1，样本外首年还受历史成分覆盖约束。因子输入可复权，成交和估值通过行情/factor恢复原价，不能以复权价格计算真实股数。

缓存 `YearMeta` 保存signature、factor_ids、Sample行及finite_counts；`.bin`为row-major little-endian f32，NaN是missing。加载核对签名、列顺序与字节形状，旧近似报价/基准字段不属于当前缓存。没有当前数据版本内容签名时先重新登记。

账户逐日先推进到账及可售状态，执行前一日信号，收盘登记权益并计算包含应收的NAV。停牌不成交，除权日调整持仓标记避免重复收益；最后一日信号可以交给下一年度。模型换代不重置现金、股数或调仓时钟，末年也不构造虚拟强制卖出。

缺实际支付/上市日、源冲突、未知复权因子跳变、配股、零碎股分配或重叠未上市动作解释失败时停止。常规板块限制仍使用0.5个百分点保守缓冲；缺少ST/上市特例、精确限价、逐笔流动性与退市清算，不能宣称完整实盘复现。

## 结果、手算与验收

完整产物表见 [ML README](../../README.md)。schema4包含双签名、公式版本、执行配置、年度报告及汇总；`validate_result`被报告、平台读取和只读导入复用。旧格式返回unsupported_result_format，不迁移或重新计费。

合成平台fixture用1005元、最低5元佣金买100股；每股分红0.1元并送0.1股，之后原价9元，两个样本外年末均为110股与现金10元，累计费用5元，总收益−5/1005。跨年延迟支付/上市fixture另外验证登记后出售、到账前不可花费/出售和除权估值。

从仓库根目录执行，原生环境见 [Crates导航](../../../README.md#开发与验收)：

```bash
cargo nextest run -p analytics-ml --features lightgbm-native,duckdb-native -E 'test(backtest::)'
cargo nextest run -p analytics-workflow -E 'test(synthetic_annual) | test(incomplete_action)'
cargo nextest run -p analytics-desktop -E 'test(research_worker) | test(unsupported_history)'
```

真实公开数据仍可能被严格准入阻止；成功的合成研究只验证流程和账本，不是中证500历史收益结论。
