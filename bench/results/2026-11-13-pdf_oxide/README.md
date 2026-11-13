# nupdf, MuPDF and PDF Oxide, 2026-11-13

`cargo xtask bench --against pdf_oxide --runs 3`, on `standard` (`--sample 200`) and on `long`. Each `summary.md` is the run's summary; each JSON file holds one operation's rows, a file each, as `xtask/src/against.rs` writes them. `text.json` and the `text` row are from a rerun of that operation alone, after the CLI's page separators were left out of its character count (`f7903f9`). The write-up is [docs/BENCHMARKS.md](../../../docs/BENCHMARKS.md).

- Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, mains power, load average 7 to 18 from other work.
- nupdf `1abb7e5`, release build. MuPDF 1.28.6 (the pinned oracle). PDF Oxide 0.3.78: the `pdf-oxide` CLI (`cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78`) and the Python bindings on CPython 3.12.11.
