# Workflow use cases

The parent dispatcher exhaustively routes WorkflowRequest variants to acquisition,
actions, research_jobs and configuration. `catalog` owns data-admission checks;
`results` interprets current artifacts. Workspace storage itself owns paths,
atomic metadata and run-state transitions, not research policy.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-workflow
```

New operations must freeze inputs, check cancellation, publish artifacts before
success and be covered through the shared service/client entry points.
