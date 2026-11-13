# nupdf, MuPDF and PDF Oxide on main at ca67177, 2026-11-13

The data behind [docs/BENCHMARKS.md](../../../docs/BENCHMARKS.md) and the speed figures in [docs/RESULTS.md](../../../docs/RESULTS.md), measured on `main` at `ca67177`, after the speed fixes. Each `summary.md` is a run's summary; each JSON file holds one operation's rows, a file each.

- `mupdf/standard`, `mupdf/long`: `cargo xtask bench --runs 3` (`--tier standard --sample 200`, `--tier long`), nupdf against `mutool`, command by command, as processes (`xtask/src/bench.rs`). Their `summary.md` keeps each pair's description and result line from the run's output.
- `engines/standard`, `engines/long`: `cargo xtask bench --against engines --runs 5` (`--sample 200` on `standard`), nupdf's library against pdf_oxide's in process (`xtask/src/engines.rs`, `bench/engines`).
- `standard`, `long`: `cargo xtask bench --against pdf_oxide --runs 3` (`--sample 200` on `standard`): MuPDF, nupdf, the `pdf-oxide` CLI and the Python bindings as processes (`xtask/src/against.rs`).

Machine and versions:

- Apple M5 Max (18 cores), 128 GB, macOS 27.2.1, mains power, busy with other work (load average about 6).
- nupdf `ca67177`, release build. MuPDF 1.28.6 (the pinned oracle). PDF Oxide 0.3.78 (MIT OR Apache-2.0): the `pdf_oxide` crate with features `rendering` and `logging` (as its CLI) for the engines; the `pdf-oxide` CLI (`cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78`); the Python bindings on CPython 3.12.11.

In `mupdf/long/render.json` and `render-threads.json`, `mutool draw` ran into the bench's 300 s timeout on `long-018.pdf` (the median of its runs is 300.0 s). The MuPDF pairs count a timeout as the time taken, so MuPDF's real time on that document is longer than recorded.
