# nupdf, MuPDF and pdf_oxide, 2026-11-13

`cargo xtask bench --against pdf_oxide --runs 3`, on `standard` (`--sample 200`) and on `long`. Each `summary.md` is the run's summary; each JSON file holds one operation's rows, a file each, as `xtask/src/against.rs` writes them. The write-up is [docs/BENCHMARKS.md](../../../docs/BENCHMARKS.md).

- Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, mains power, load average 6 to 10 from other work.
- nupdf `b028ca9`, release build. MuPDF 1.28.6 (the pinned oracle). pdf_oxide 0.3.78 on CPython 3.12.11.
