# Research stages

`stages::run` composes private typed transitions. Each phase receives an explicit
`TaskContext`; native resources remain owned by the calling task thread.

- `prepare`: resolve dates and frozen membership, validate provenance and input admission.
- `sign`: select ordered features and freeze factor/input and execution provenance.
- `replay`: validate immutable predictions and run the account without loading models.
- `samples`: prepare nullable factor shards, sample masks and training signature; audit stops here.
- `fit_predict`: fit or reuse fold models and produce dated signals without resetting accounts.
- `publish`: publish model receipts, predictions, then execute one continuous account.

`PreparedResearch`, `SignedResearch`, `FeatureSamples` and `FoldOutputs` own phase
outputs. A phase returning `None` means a completed audit or prediction replay;
it never falls through to training. `artifacts.rs` defines typed training,
prediction, sample and result contracts. Dynamic source evidence remains JSON.
Hashing frozen factor references uses canonical JSON values, preserving the
existing feature schema signature rather than Rust struct declaration order.

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-workflow
```

No factor formula, null policy, feature ordering, fee or account arithmetic is
changed by phase extraction. Existing per-mode policy is retained inside the
owning phase rather than a single monolithic executor.
