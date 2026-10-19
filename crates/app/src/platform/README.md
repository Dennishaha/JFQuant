# Workspace panels

The three-factor area also submits CollectThreeFactorInputs with a default-off provider-date
option and the same input file picker. Its returned artifact can feed BuildThreeFactors.
Both tasks use the shared worker and prevent duplicate submissions; the temporal assumption
remains explicit in the UI and published artifacts.

`data` also owns the historical-facts file picker and three-factor action; it submits
`BuildThreeFactors` to the shared workflow worker, with no financial calculation on the UI thread.

The platform root contains shared editor state and request submission. Workspace,
data, research and run-history rendering live in separate modules.

`reads` loads catalog, result, NAV and log snapshots on background threads through
workflow read models. Responses carry workspace/selection identity; late responses
cannot replace a newer selection. A record's stage and completion timestamp
invalidate its cached view. Import validation and service requests also run off
the render thread. Local configuration dialogs remain client responsibilities.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-desktop
```

Interaction acceptance waits for asynchronous result/import errors and checks
unsupported results never render a NAV chart.

## 当前阻断与历史证据

“证据维护”支持指定已结束的训练/回测任务ID检查所选行动版本，展示当前覆盖可用/部分覆盖/覆盖阻断、剩余区间与类别、事件检查和补齐前后变化。历史采集失败独立折叠展示，不计入当前阻断数。版本、任务或所选市场改变时旧报告标为过期。完整训练/回测仍须重新准入。

“完整历史覆盖审核”复用修订JSON的 `coverage_reviews`，展示日期、类别、候选数和理由，沿用草稿校验、载入与提交新版本操作。界面不自行清除失败或批准完整历史。验证：`DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-desktop`；打包后执行 `crates/app/packaging/verify-cli.sh APP OUTPUT`（包含离线任务范围证据检查）。
