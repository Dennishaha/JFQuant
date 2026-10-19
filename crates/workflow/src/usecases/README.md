# Workflow use cases

The parent dispatcher exhaustively routes WorkflowRequest variants to acquisition,
actions, research_jobs, configuration and the three_factor_build component. `catalog` owns data-admission checks;
`results` interprets current artifacts. Workspace storage itself owns paths,
atomic metadata and run-state transitions, not research policy.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-workflow
```

New operations must freeze inputs, check cancellation, publish artifacts before
success and be covered through the shared service/client entry points.

Storage release hides unavailable versions without rewriting catalog receipts. Re-registering restored identical bytes creates a fresh available identity and remains idempotent; historical task inputs continue to refer to the released identity. Storage task results expose the typed report directly. See [storage management](../storage_management/README.md).
