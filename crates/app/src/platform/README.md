# Workspace panels

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
