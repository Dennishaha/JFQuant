# Architecture acceptance

Run from the repository root with Python 3.9+, Cargo and Git:

```bash
python3 crates/workflow/checks/architecture.py
python3 -B -m unittest discover -s crates/workflow/checks -p 'test_execution_dependencies.py'
```

The check rejects upward crate dependencies, cross-component embedded resources,
client access to worker internals, business functions in the workflow root, and
nested task workers used as query sessions. Long new or growing production
functions produce review notices, not automatic failures. Tests are excluded.
The function-size heuristic is deliberately approximate; a notice requires a
responsibility review rather than arbitrary line splitting.

The execution dependency receipt is owned by workflow and replaces embedding the
workspace manifest in production Rust. It binds Cargo.lock and normalized effective
Cargo contracts for workflow and its local dependency closure, including inherited
requirements and selected feature declarations. Paths do not enter the receipt.
The architecture gate runs `cargo metadata --locked --offline --no-deps` to check it;
no registry downloads or Python packages are needed. Refresh it after dependency
changes with `python3 crates/workflow/checks/execution_dependencies.py --write`.
Existing signed tasks keep their frozen execution identities; no task, model or
market manifest is migrated by this build-time dependency receipt.

Run this alongside workspace nextest, Clippy, Rustdoc and formatting checks.

For the ordered full suite after the runtime has been built:

```bash
bash crates/workflow/checks/acceptance.sh
```

Do not rebuild/package the shared Python environment concurrently with this suite;
training/GUI tests use bounded deadlines and require a stable interpreter tree.


`benchmark-json-storage.py --workspace /absolute/workspace --mode json|sqlite [--iterations 5]`
只读比较迁移前后Catalog头信息与当前代次运行请求查询，中位耗时和进程峰值RSS以JSON输出。
两种模式分进程运行，不清系统缓存；应在离线稳定样本上执行并将输出保存在工作区外。
完整盘点仍需单独测量；[方法及迁移步骤](../src/storage_management/README.md#json元数据拆表协议47)。
