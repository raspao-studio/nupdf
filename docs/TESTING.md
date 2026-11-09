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
cargo xtask corpus list                 # what is fetched, sizes, licenses
cargo xtask corpus fetch qpdf           # sparse clone of one source
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
| `streams` | BLAKE3 of each decoded stream | exact, except image filters | M2 |
| `roundtrip` | MuPDF reading nupdf's output: same graph modulo renumbering, same text | exact | M3 |
| `trace` | Device calls per page, against `mutool trace` | matrices within 1e-3 | M4 |
| `stext` | Characters, bboxes and fonts, against MuPDF's stext JSON | IoU ≥ 0.98 | M5 |
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

Parity is `match` divided by the files MuPDF handled. Reports go to `target/harness/<tier>/` (`summary.md` plus one JSON file per check, with MuPDF's warnings per file).

`harness/baselines/<tier>.json` records each file's outcome. A run fails if any file that matched in the baseline no longer matches. Newly matching files are reported, and `--update-baseline` locks them in. A baseline recorded with a different oracle version is refused.

## Benchmarks

- Pairs run the same task with `mutool` and with `nupdf`, end to end as processes, because that is what an agent pays for. One warmup run, then N timed runs. The report gives median wall time, peak RSS (from `wait4`) and the geometric-mean speedup.
- Runs are sequential, on the local bench machine. CI runners are too noisy to gate on.
- From M5, context pairs add PyMuPDF, pypdf, pdfplumber, pdfminer.six, pdftotext and pdf_oxide, so the agent-facing comparison is concrete.
- Scaling runs repeat each page-parallel command at 1, 2, 4, 8 and N threads.
- Results that support public claims are committed under `bench/results/` with machine details.

The M0 `info` pair is not like for like (nupdf only reads the header). It exists to exercise the runner. Its numbers mostly measure process startup: about 2 ms for nupdf and 5 ms for mutool.

## Fuzzing (from M1)

```bash
cargo +nightly fuzz run -O open_walk -- -max_total_time=3600   # one CPU-hour
cargo +nightly fuzz run -O lex                                  # until stopped
```

Targets live in `fuzz/fuzz_targets`: `lex`, `parse_object`, `filters` and `open_walk` (open a document, compute its version and page count, load every object and decode every stream). Seed `fuzz/corpus/open_walk/` with the smoke fixtures and small standard-tier files; corpora and crash artifacts are not committed.

- cargo-fuzz targets per parser surface: `lex`, `parse_object`, `open_walk`, one per decoder, the content parser and the font parsers.
- Seeds come from the smoke and standard tiers.
- Every crash becomes a smoke fixture or a unit test before it is fixed.
- A milestone closes only after one CPU-hour per new target with no findings. Longer runs happen locally overnight, and OSS-Fuzz is the goal for M10.

## Agent eval (M8)

About 30 tasks with automatic graders, run with Claude Code in a clean environment per arm. Arms: nupdf CLI, nupdf MCP, the default Python PDF skill (pypdf and pdfplumber), pymupdf4llm-mcp, and pdf_oxide. Metrics: graded success, wall time, tool calls and tokens. Task files, graders and results are committed, so anyone can rerun them.

## CI

- `ci.yml` on every push and pull request: rustfmt, clippy with warnings as errors, `cargo test --workspace`, fixture freshness, and cargo-deny (licenses, advisories, sources).
- `harness.yml` on every push, pull request and manual dispatch: builds the oracle (cached by version), runs the oracle-dependent tests and the smoke-tier harness, and uploads the reports.
