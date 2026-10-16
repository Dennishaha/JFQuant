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
