# analytics-factor

## 职责

analytics-factor 注册并解析 GTJA/DolphinDB Alpha191 公式，执行时间序列和横截面算子，输出宽表因子缓存。公式版本固定为 dolphindb-gtja191-2023.01.17。

## 公共接口

| 接口 | 契约 |
|---|---|
| FactorRegistry | 提供 191 个 canonical ID、公式、输入列和 lookback |
| FactorExecutor::compute | 校验 panel 和请求，发送 Started/Progress/Finished，按因子检查取消 |
| Panel::validate | 要求 instrument、date、每个数值列等长 |
| next_return_labels | 每个 instrument 按日期取下一条 close return，末行为 null |
| FactorRunArtifact::write_parquet_with_labels | 保留 nullable factor，按 factor ID 顺序写列和 manifest |
| unsupported_ids | 返回仍需算子或源公式修正的注册项 |

缺失历史、输入 null、零除等情况保持 null；不能前向填充。训练适配器才会把 null 映射为 LightGBM 的 missing/NaN。

## 数据流

    FactorRegistry -> parser/FactorExpr -> InMemoryFactorExecutor -> FactorRunArtifact -> factors.parquet

## 构建与验收

    cargo check -p analytics-factor
    cargo test -p analytics-factor
    cargo test --doc -p analytics-factor
    cargo clippy -p analytics-factor --all-targets -- -D warnings

验收要求：191 个公式全部能解析；fixture/golden harness 覆盖执行结果；next_return_labels 按 instrument 隔离且尾行 null；取消不会跨线程移动面板或句柄。

