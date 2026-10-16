# Architecture acceptance

Run from the repository root with Python 3.9+:

```bash
python3 crates/workflow/checks/architecture.py
```

The check rejects upward crate dependencies, cross-component embedded resources,
client access to worker internals, business functions in the workflow root, and
nested task workers used as query sessions. Long new or growing production
functions produce review notices, not automatic failures. Tests are excluded.
The function-size heuristic is deliberately approximate; a notice requires a
responsibility review rather than arbitrary line splitting.

Run this alongside workspace nextest, Clippy, Rustdoc and formatting checks.

For the ordered full suite after the runtime has been built:

```bash
bash crates/workflow/checks/acceptance.sh
```

Do not rebuild/package the shared Python environment concurrently with this suite;
training/GUI tests use bounded deadlines and require a stable interpreter tree.
