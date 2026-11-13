# nupdf, MuPDF and PDF Oxide, 2026-11-13

The data behind [docs/BENCHMARKS.md](../../../docs/BENCHMARKS.md). Each `summary.md` is a run's summary; each JSON file holds one operation's rows, a file each.

- `engines/standard`, `engines/long`: `cargo xtask bench --against engines --runs 5` (`--sample 200` on `standard`), nupdf's library against pdf_oxide's in process (`xtask/src/engines.rs`, `bench/engines`).
- `standard`, `long`: `cargo xtask bench --against pdf_oxide --runs 3` (`--sample 200` on `standard`): MuPDF, nupdf, the `pdf-oxide` CLI and the Python bindings as processes (`xtask/src/against.rs`). `text.json` and the `text` row are from a rerun of that operation alone, after the CLI's page separators were left out of its character count (`f7903f9`).

Machine and versions:

- Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, mains power, load average 7 to 18 from other work.
- nupdf `1abb7e5`, release build. MuPDF 1.28.6 (the pinned oracle). PDF Oxide 0.3.78 (MIT OR Apache-2.0): the `pdf_oxide` crate with features `rendering` and `logging` (as its CLI) for the engines; the `pdf-oxide` CLI (`cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78`); the Python bindings on CPython 3.12.11.
