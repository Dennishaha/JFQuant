# Collector boundaries

The parent module owns public collection requests, provider routing and runtime
identity. `process` owns supervised subprocess transport and checkpoints;
`publication` verifies completed artifacts and publishes source receipts.
Source-specific parsing and reconciliation remain in their existing components.
No subprocess/native handle is shared with the rendering thread.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-storage
PYTHONPATH=crates/storage/collectors target/collector-build/python/bin/python3 -B -m unittest discover -s crates/storage/collectors/tests
```
