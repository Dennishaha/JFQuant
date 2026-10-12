# Repository instructions

## Documentation

- Keep component documentation beside the component code. Do not create a standalone `docs/` directory.
- Every crate must maintain a `README.md` describing responsibility, public interfaces, data flow, build commands, and acceptance checks.
- Update the component README whenever a public interface, file format, model manifest, or async task changes.
- Prefer Mermaid diagrams for architecture and data flow.
- Record reproducible commands and environment variables in documentation.
- Do not commit generated datasets, model files, native libraries, or temporary output.

## Engineering rules

- Keep DuckDB and LightGBM handles inside their worker thread; they are not shared across UI threads.
- Preserve feature order, schema hashes, and formula versions in manifests.
- Treat missing values as data: factor execution emits nulls and the model adapter maps them to LightGBM missing values.
- New factor implementations require a fixture or golden-output test.

