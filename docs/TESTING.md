# Testing

nupdf's claims (parity, safety, speed, agent usefulness) are only as good as the measurements behind them. This file describes each layer of testing, how to run it, and how results are ratcheted.

## Commands

```bash
cargo test --workspace                  # unit, integration and fixture-freshness tests
cargo xtask oracle build                # build the pinned MuPDF once (about 15s on an M-series Mac)
cargo test --workspace -- --ignored     # tests that need the oracle
cargo xtask fixtures                    # regenerate corpus/smoke (add --check to verify)
cargo xtask harness                     # differential checks on the smoke tier
cargo xtask harness --tier standard     # same on fetched public corpora
cargo xtask harness --update-baseline   # accept improved results
cargo xtask bench --runs 5              # paired timing and memory against mutool
cargo xtask bench --tier standard --sample 300 --runs 3   # evenly spaced sample of a big tier
cargo xtask bench --tier standard --sample 200 --runs 3 --against pdf_oxide   # MuPDF, nupdf and pdf_oxide
cargo xtask corpus list                 # what is fetched, sizes, licenses
cargo xtask corpus fetch qpdf           # sparse clone of one source
cargo xtask crashes --tier wild         # nupdf alone on every file: crashes, internal errors, timeouts
cargo llvm-cov --workspace --summary-only --ignore-filename-regex '(nupdf-cli|xtask|fuzz)/'   # line coverage of the nupdf crate (PLAN: at least 90%)
```

## Layers

| Layer | What it proves | Where |
|---|---|---|
| Unit tests | Each function's behavior, including MuPDF's edge cases | next to the code |
| CLI integration tests | The JSON contract, exit codes and error codes | `crates/nupdf-cli/tests` |
| Smoke fixtures | Generated PDFs, one feature or failure each, committed and checked for freshness | `corpus/smoke`, `xtask/src/fixtures.rs` |
| Differential harness | Same results as MuPDF, file by file, on real corpora | `xtask/src/harness.rs`, `oracle/js` |
| Property tests | Round trips: serialize then parse is the identity (from M1) | `crates/nupdf` |
| Fuzzing | No panics, hangs or unbounded allocation on any input (from M1) | `fuzz/` |
| Benchmarks | Speed and peak memory against MuPDF and others | `xtask/src/bench.rs` |
| Agent eval | Agents do better with nupdf than with the alternatives (M8) | `eval/` |

## The oracle

- The pin is `oracle/MUPDF_VERSION` (1.28.6). `cargo xtask oracle build` shallow-clones that tag with submodules into `target/oracle/mupdf-<version>/` and builds the command-line tools without X11, GLUT or curl.
- `NUPDF_MUTOOL=/path/to/mutool` overrides the location, but the binary must report the pinned version. Results are only comparable within one MuPDF version.
- The oracle runs out of process: a MuPDF crash or hang becomes a classified outcome, not a harness failure.
- Oracle scripts live in `oracle/js/` and run as `mutool run <script> <file>`. They print one JSON object whose field names match nupdf's, so the comparison is a field-by-field equality, with tolerances where a check defines them.
- The MuPDF source tree in `target/oracle/` is also the reference for porting work.
- The `qpdf-*` checks also need qpdf on `PATH`, or `NUPDF_QPDF=/path/to/qpdf`. Any recent version does: both sides' output is checked by the same binary, so its messages only have to agree with themselves. CI installs Ubuntu's.

**Bumping the oracle:** change `MUPDF_VERSION`, rebuild, rerun every tier, and review upstream changes to the files in ARCHITECTURE's module map. Then re-baseline with `--update-baseline` in a commit that explains the differences.

## Corpus tiers

| Tier | Contents | Where |
|---|---|---|
| `smoke` | 32 generated fixtures, one per syntax feature, leniency or repair path; growing with each milestone | committed in `corpus/smoke` |
| `standard` | pdf.js, qpdf, PDFium, veraPDF, py-pdf sample files, pdf-differences, pdf20examples: about 5k files | fetched into `$NUPDF_CORPUS_DIR` |
| `redaction` | pdf-redaction-benchmarks, plus nupdf's generated leak fixtures | fetched, from M6 |
| `wild` | Samples of GovDocs1 and SafeDocs CC-MAIN-2021-31 | downloaded by hand, from M1 |

Third-party PDFs are never committed; `corpus/manifest.toml` records each source's URL, license and purpose. The wild tier contains malformed and malicious files. Neither tool executes PDF JavaScript, but run wild-tier jobs on a machine you don't mind exposing to hostile parsers' bugs.

## Checks

| Check | Compares | Tolerance | From |
|---|---|---|---|
| `info` | Effective version, page count, xref size, repaired, revisions, Info metadata; encryption later | exact | M0, extended in M1 |
| `objects` | Every object in canonical JSON (names `#xx`-escaped, strings hex, reals at f32), plus raw stream lengths | exact (numbers per DECISIONS D13) | M1 |
| `pages` | Page lookup for every index and inherited MediaBox, CropBox, Rotate and Resources; page count before and after lookups | exact | M1 |
| `warnings` | Distinct `warning:` and `<kind> error:` lines on MuPDF's stderr against nupdf's diagnostic messages, while running `info` | set equality | M1 |
| `streams` | BLAKE3 of each decoded stream | exact, except image filters | M2 |
| `clean` | BLAKE3 of the `clean` output against MuPDF's save in reproducible mode, the writer's comment line swapped | exact | M3 |
| `clean-g` … `clean-gggg` | The same at each garbage collection level | exact | M3 |
| `clean-d`, `clean-z`, `clean-ze`, `clean-a`, `clean-dfi`, `clean-dzgg` | The same with stream options: decompress, compress (at the default effort and at 100), hex, decompress all but fonts and images, and decompress and compress after renumbering | exact | M3 |
| `clean-Z`, `clean-Zg`, `clean-Zggz` | The same with object streams and an xref stream: alone, at `-g`, and with `-gg` and `-z` | exact | M3 |
| `clean-D` | The same without encryption | exact | M3 |
| `clean-pages-reverse`, `clean-pages-first`, `clean-pages-rest` | The same keeping pages `N-1`, `1` and `2-N` (`pdf_rearrange_pages`), every object loaded first as `mutool clean` does | exact | M3 |
| `roundtrip-E40`, `roundtrip-E128`, `roundtrip-Eaes128`, `roundtrip-Eaes256` | MuPDF reading back each side's output encrypted with a new handler (owner password only): objects decrypted, raw stream data, repair, warnings. Bytes differ by design: MuPDF's new /ID and AES secrets are random | exact | M3 |
| `roundtrip`, `roundtrip-gggg` | MuPDF reading back each side's `clean` (or `clean -gggg`) output: objects, raw stream data, repair, warnings | exact | M3 |
| `trace` | Device calls per page, against `mutool trace` | matrices within 1e-3 | M4 |
| `stext` | Each page's blocks, lines and characters (text, font, flags, bidi level, origin, size, quad) against `Page.toStructuredText` walked by oracle/js/stext.js, with default options; the summary adds the M5 measures (pages with MuPDF's text, characters with box IoU ≥ 0.98) | same structure and text, IoU ≥ 0.98 | M5 |
| `text`, `text-dehyphenate` | Each page's text from `extract --fields text` against `asText()` (`fz_print_stext_page_as_text`), with default options and with `dehyphenate` | exact | M5 |
| `roundtrip-text` | MuPDF reading back each side's `clean` output: each page's text, as `text` gives it | exact | M5 |
| `search`, `search-icase`, `search-regexp` | Each page's hits of `the`; of `THE` ignoring case and diacritics; of `[0-9]+` as a regular expression; against `StructuredText.search` (oracle/js/search.js), each hit's quads | numbers within a few float steps | M5 |
| `stext-preserve`, `stext-raw` | The same with ligatures, whitespace and spans kept, images and shadings as blocks and hyphens joined; and with no added spaces, CIDs for unknown Unicode, ActualText ignored, no clipping and outline boxes | same structure and text, IoU ≥ 0.98 | M5 |
| `redact` | Remaining text against MuPDF's redaction; zero targets for nupdf, MuPDF and pdftotext | exact | M6 |
| `render` | Pixels against `mutool draw` at 72 and 150 dpi | threshold per page | M7 |

## Outcomes and the ratchet

Each file in a check gets one outcome:

| Outcome | Meaning |
|---|---|
| `match` | Both succeeded and every compared field agrees |
| `mismatch` | Both succeeded and some field differs; the report shows each field's two values |
| `nupdf_failed` | MuPDF handled the file and nupdf did not |
| `oracle_failed` | nupdf answered and MuPDF did not. This is not automatically good; each one gets reviewed |
| `both_failed` | Both rejected the file |
| `timeout` | Either side exceeded `--timeout` |
| `crash` | Either side died from a signal. For nupdf this is always a bug |

Some checks validate output instead of comparing it: `qpdf-*` run `qpdf --check` on each side's output and compare its verdict and messages (qpdf must be installed, or `NUPDF_QPDF` set to it), and `arlington-*` apply selected Arlington PDF Model rules to the dictionaries nupdf writes (DECISIONS D28). Either way nupdf's output matches when the validator says the same of it as of MuPDF's. `cargo xtask arlington <files>` prints the violations it finds in any file.

Parity is `match` divided by the files MuPDF handled. Reports go to `target/harness/<tier>/` (`summary.md` plus one JSON file per check, with MuPDF's warnings per file). A capital in a check's name becomes `_` and the letter in its file name, so `clean-Z` and `clean-z` stay apart on a case-insensitive filesystem.

`harness/baselines/<tier>.json` records each file's outcome. A run fails if any file that matched in the baseline no longer matches. Newly matching files are reported, and `--update-baseline` locks them in. A baseline recorded with a different oracle version is refused.

## Benchmarks

- Pairs run the same task with `mutool` and with `nupdf`, end to end as processes, because that is what an agent pays for. One warmup run, then N timed runs. The report gives median wall time, peak RSS (from `wait4`) and the geometric-mean speedup.
- Runs are sequential, on the local bench machine. CI runners are too noisy to gate on.
- From M5, context pairs add PyMuPDF, pypdf, pdfplumber, pdfminer.six, pdftotext and pdf_oxide, so the agent-facing comparison is concrete.
- `--against pdf_oxide` (`xtask/src/against.rs`, `oracle/bench/oxide.py`) runs the operations pdf_oxide also has (`info`, `text`, `text-fast`, `render`, `tables`, `annots`, `forms`, `signatures`) on the same files with MuPDF, nupdf and pdf_oxide's Python bindings from `target/bench-venv`. pdf_oxide is timed as a process and in process (opening the document to its last byte of output, the interpreter and import left out); nupdf's in-process time is its process time less that of `nupdf --version`. A side that exits with an error, or for pdf_oxide raises on a page, has failed on the file. Where nupdf's and pdf_oxide's outputs must agree (page count, whether a file has text, how many pages have ink), a file on which they do not is counted apart. Ratios are over the files every side did and agreed on. Reports go to `target/bench/<tier>/against-pdf_oxide/`; numbers are in [BENCHMARKS](BENCHMARKS.md).
- Scaling runs repeat each page-parallel command at 1, 2, 4, 8 and N threads.
- Results that support public claims are committed under `bench/results/` with machine details.

The M0 `info` pair is not like for like (nupdf only reads the header). It exists to exercise the runner. Its numbers mostly measure process startup: about 2 ms for nupdf and 5 ms for mutool.

## Fuzzing (from M1)

```bash
cargo +nightly fuzz run -O open_walk -- -max_total_time=3600   # one CPU-hour
cargo +nightly fuzz run -O lex                                  # until stopped
```

Targets live in `fuzz/fuzz_targets`: `lex`, `parse_object`, `filters`, `crypt`, `images`, `stext` (pages gathered into structured text with options the first byte picks), `encode` (the deflate and CCITT G4 encoders `clean` writes with, each output decoded back) and `open_walk` (open a document, compute its version and page count, load every object, decode every stream, and write it as `clean` does with options the input picks). Seed `fuzz/corpus/open_walk/` with the smoke fixtures and small standard-tier files; corpora and crash artifacts are not committed.

- cargo-fuzz targets per parser surface: `lex`, `parse_object`, `open_walk`, one per decoder, the content parser and the font parsers.
- Seeds come from the smoke and standard tiers.
- Every crash becomes a smoke fixture or a unit test before it is fixed.
- A milestone closes only after one CPU-hour per new target with no findings. Longer runs happen locally overnight, and OSS-Fuzz is the goal for M10.

## Agent eval (M8)

About 30 tasks with automatic graders, run with Claude Code in a clean environment per arm. Arms: nupdf CLI, nupdf MCP, the default Python PDF skill (pypdf and pdfplumber), pymupdf4llm-mcp, and pdf_oxide. Metrics: graded success, wall time, tool calls and tokens. Task files, graders and results are committed, so anyone can rerun them.

## CI

- `ci.yml` on every push and pull request: rustfmt, clippy with warnings as errors, `cargo test --workspace`, fixture freshness, and cargo-deny (licenses, advisories, sources).
- `harness.yml` on every push, pull request and manual dispatch: builds the oracle (cached by version), runs the oracle-dependent tests and the smoke-tier harness, and uploads the reports.
