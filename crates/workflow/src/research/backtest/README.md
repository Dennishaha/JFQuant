# Dated research

`rights`将格式8部分配股事实投影到固定原生AKQuant状态机；`account`独立重放登记、认购、退款、上市及锁定资产估值。App/CLI共用BacktestPredictions，默认放弃认购；主动选择参与时，默认首日一次、现金内部分认购；实际持仓缺必要字段停止。原生账户使用共享配股快照，避免订单预算反复深拷贝增长中的审计记录；估值只查有配股资产的证券，锁定价值不增加购买力。

结果17保留读取16；账本rights-v2与旧v1分别核验。预测复用验证冻结来源、原预测及样本缓存哈希，不读取模型，不绕过原模型的运行时校验。普通成交、费用、T+1、成交量限制沿用上游固定版。配股费用与本金单独记录，不能伪装成普通市场成交。

`data` prepares the unchanged Rust factor families and dated sample masks;
`quality` qualifies the universe without future-dependent exclusions; `adjustment`
verifies training coefficients independently of account settlement dates.

`akquant` converts raw bars, frozen membership and sample-out-of-fold scores to the
Python worker. It rejects known unsupported events and nonrepresentable fees.
`portfolio` computes reporting statistics only; it does not simulate an account.
`report` renders the schema14 AKQuant semantics and upstream fee totals.

All folds feed one backtest. Report gaps and calendar years do not reset positions.
Missing execution prices are not filled with zero or a different price field.
Manifests retain feature order, hashes, formula and runtime versions.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-workflow research::backtest
```

HiThink真实账户验收：年度因子分片按逐证券供应商口径读取已前复权OHLC、原始股数与CNY金额，指数同样不要求独立系数；中性缩放不写成研究系数，VWAP继续为null。通达信/Qlib的既有单位恢复保留。fixture覆盖股票、指数、null VWAP及两种量额转换路径。

混合因子选择来自ResearchConfig.factor_refs，prepare_year按最大嵌套lookback扩大历史读取，
算子经统一注册表执行。缓存分片manifest记录载入历史起点和递归种子约定；训练描述4保留完整固定修订。
旧模型复用不读取可变自定义因子库；验证定义版本、顺序、模型哈希和训练签名后继续已有账户执行流程。

## 独立训练和跨池执行

`independent.rs` 定义计划、模型收据、切换计划和共享App/CLI操作；`independent_tests.rs` 包含隔离、时间安全、手算排名、缓存、历史特征稳定及无模型预测重放验收。执行适配器把新阶段接入同一因子/账户内核，原Schedule路径不变。训练发布5、结果19兼容16—18；按实际A/B字段分别签名。详细格式和命令见workflow根README。
