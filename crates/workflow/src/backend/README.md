# Task execution and queries

`query::QuerySession` owns one DuckDB connection and pagination snapshot. The
worker owns a map of sessions with a ten-minute idle expiry; all handles are
created and released on that worker thread. Sessions never own task state.

`lifecycle` resolves durable records before Started, catches panics, determines
terminal status, validates results, persists terminal records and emits completion.
Terminal records cannot return to Running or acquire a conflicting terminal state.
`TestWorker` is available only under test/test-support; production Worker fields
are private and clients exchange commands/events.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-workflow -E 'test(backend)'
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-cli --test platform
```


进度回调通过`Workspace::task_progress`按UUID追加events并只更新SQL stage，避免为每个因子/
训练迭代读取、JSON投影和重解析完整冻结请求。扫描写锁忙时阶段更新可跳过，事件仍保留；
终态写入仍必须成功。完整迁移与验收见[存储管理](../storage_management/README.md#json元数据拆表协议47)。
