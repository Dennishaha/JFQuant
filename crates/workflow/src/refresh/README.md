# Refresh boundaries

The parent module defines request/plan contracts and source policy. `planning`
resolves missing historical ranges, `quotes` performs bounded quote acquisition,
`actions` validates independent action evidence, `publication` reuses verified
shards and publishes immutable versions, and `sources` implements source probes.
All spawned acquisition threads inherit the explicit task context. Cancellation
retains verified checkpoints; collection success does not imply research readiness.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-workflow -E 'test(refresh)'
```
