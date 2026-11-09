# nupdf

A Rust rewrite of MuPDF's PDF engine, built for agents. Read `docs/PLAN.md` first. Work proceeds milestone by milestone through its checkboxes.

## Commands

- `cargo test --workspace`, then `cargo test --workspace -- --ignored` (needs the oracle)
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all`
- `cargo xtask oracle build`: pinned MuPDF in `target/oracle/mupdf-<version>/`. Its `source/` tree is the reference to read when porting.
- `cargo xtask fixtures`: regenerate `corpus/smoke`. Never edit those files by hand.
- `cargo xtask harness [--tier standard] [--check NAME] [--update-baseline]`
- `cargo xtask bench [--pair NAME] [--runs N]`

## Rules

- Follow the work-item loop in PLAN.md: tests first, implement, harness, tick the box in the same commit.
- Product crates never use `unsafe`, and never `unwrap`/`expect`/`panic!` on paths that touch input. Loops and recursion over input go through `Limits`.
- Default behavior matches MuPDF. Better behavior is an opt-in mode (DECISIONS D5).
- When a module follows a MuPDF file, add the header used in `crates/nupdf/src/header.rs`: SPDX line, our copyright, "Derived from MuPDF <path>", Artifex's copyright. Also add the file to the module map in ARCHITECTURE.md.
- Every command returns a versioned report with `schema`, `doc_id` and `diagnostics` (AGENT-INTERFACE.md). New warnings get stable dotted codes.
- Add a fuzz target with every new parser surface.
- Update a baseline only when results improved, never to hide a regression.
- New dependencies must be pure Rust with licenses on the `deny.toml` allow-list.
