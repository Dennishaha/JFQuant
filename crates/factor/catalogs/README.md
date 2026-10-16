# Immutable builtin factor catalogues

Both JSON files use catalogue schema 1: explicit ordered IDs, name, original formula, syntax, optional source AST, formula version and source metadata. They are embedded with `include_str!`; workspace users cannot edit builtin definitions. GTJA source strings and Qlib source ASTs preserve their prior definitions exactly. `src/formulas.rs` validates catalogue versions, and the unified operator registry compiles both into explicit semantic operator IDs before execution.

Acceptance: `cargo test -p analytics-factor`; both independent golden fixtures must remain unchanged. This folder contains source definitions, never generated market data. See the component README for provenance and numeric conventions.
