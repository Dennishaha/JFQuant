# Dated research

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
