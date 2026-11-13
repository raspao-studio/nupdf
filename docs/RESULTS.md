# Results

What nupdf has been measured to do, against MuPDF (the reference it is checked against) and against the other ways an agent can handle a PDF. Each section names the command that reproduces it and the document with the full numbers. Measured on 2026-11-13 on an Apple M-series Mac, against MuPDF 1.28.6.

## In one paragraph

nupdf reads PDFs as MuPDF does, in Rust with no `unsafe` code. On the standard tier, 5,142 test files, 91 of its 97 checks against MuPDF agree on at least 99% of the files and the median check on 99.88%. Across 1.5 million runs on 100,000 files from the open web, it never crashed. Its library is faster than PDF Oxide's in geometric mean on every operation compared, and its commands faster than MuPDF's on most. In an agent eval of 30 tasks, agents using nupdf solved every task in every run, the only arm to do so. Over MCP they spent fewer tokens and less time than agents using any other tool. Over the CLI they spent slightly more tokens than agents writing PyMuPDF or pypdf scripts.

## Parity with MuPDF

`cargo xtask harness --tier standard` runs each check on every file with both engines and compares the results. The tier is 5,142 files from the pdf.js, PDFium, qpdf, veraPDF, pdf-differences and other public test suites. A check's parity is the share of matches among the files at least one side reads.

| Check | What is compared | Matches | Parity |
|---|---|---|---|
| objects, info, pages, streams | every object; document and page facts | 5,122–5,123 of 5,123 | 99.98–100% |
| text, stext, search | text as `mutool draw -F txt`, structured text with boxes and fonts, search hits | 5,123 of 5,123 | 100% |
| fonts, tables, attachments, outline, links | as MuPDF lists them | all | 100% |
| annots, forms, signatures | each annotation's getters, each widget, each signature's digest check | all | 100% |
| images | each image's properties | 5,105 of 5,123 | 99.65% |
| clean, flatten | the written file, byte for byte | 5,020 and 5,011 of 5,041 | 99.58% and 99.40% |
| merge | the written file | 5,123 of 5,123 | 100% |
| redact-text | the text left after redaction | 5,052 of 5,066 | 99.72% |
| render-72, render-150 | each page's pixels within the threshold (a channel off by more than 8, on at most 0.5% of pixels) | 5,070 and 5,069 of 5,123 files; 99.75% of 19,790 pages at 72 dpi | 98.97% and 98.95% of files |

All 97 checks, including colour-managed rendering, round trips and every editing command: [TESTING](TESTING.md). What nupdf does differently on purpose is in [DECISIONS](DECISIONS.md): D5 sets the rule that the default behaviour is MuPDF's and anything better is an option.

**On the open web** ([WILD-TIER](WILD-TIER.md)), on an evenly spaced 1,000 of 100,000 unsanitized files:
- Every check but rendering is at 99.3% or better.
- Rendering at 72 dpi matches on 98.7% of pages (95.6% of files).
- The pages left are stroked text, which MuPDF draws with FreeType's stroker (97 pages), and CJK text in fonts the file does not embed, which MuPDF draws with a 25 MB font nupdf does not carry (70 pages).

## Safety

- **No `unsafe`.** The workspace forbids it (`unsafe_code = "forbid"`). The parsers it depends on (zlib-rs, hayro-jbig2, hayro-jpeg2000, moxcms) are pure Rust.
- **No crash on the open web.** `cargo xtask crashes --tier wild` ran 15 commands on each of 100,000 files from SafeDocs' CC-MAIN-2021-31 crawl, malware included: 1,500,000 runs.
  - No run ended in a signal.
  - The 62 panics were one bug, now fixed.
  - Every run that took over a minute has been profiled and fixed, or finishes.
- **Fuzzed.** 19 fuzz targets, one per parser surface, run in CI. The files for OSS-Fuzz are ready for when the repository is public.
- **Tested.** 95.6% line coverage of the library (`cargo llvm-cov`).
- **Redaction that is checked.** Results:
  - On the adversarial suite and the redaction tier, no leaks, as judged by `nupdf verify`, MuPDF's extraction and `pdftotext`.
  - Files seeded to leak are caught by `verify`.
  
  What nupdf promises and what it does not: [THREAT-MODEL](THREAT-MODEL.md).

## Speed

`cargo xtask bench`, single-threaded, medians of 3 runs ([BENCHMARKS](BENCHMARKS.md)).

**Against MuPDF**, per command, start included:
- **Small files (200):** nupdf is 1.3x to 2.1x faster.
- **Documents of about 100 pages (30):** 1.4x to 2.9x faster on every operation but `info`. `info` scans each page for scanned pages and was 6x slower than MuPDF's; since the speed fixes of raspao-studio/nupdf#73 it takes 0.02 to 0.17 s per document, against up to 2.6 s.

**Against PDF Oxide** 0.3.78, library against library: both crates in one Rust program, timing only the work on a file already in memory. The figures are PDF Oxide's time over nupdf's, in geometric mean (above 1x, nupdf is faster):

| Operation | 200 small files | 30 long documents |
|---|---|---|
| open | 2.14x | 4.76x |
| page sizes | 2.22x | 7.77x |
| text | 1.30x | 1.42x |
| render (72 dpi, pixels only) | 1.57x | 1.61x |
| tables | 1.25x | 1.10x |
| annotations | 2.19x | 15.4x |

**Per call against PDF Oxide's own CLI:** nupdf is 1.15x to 1.9x faster on small files, and 1.57x on long documents' text.

**Failures:** PDF Oxide fails 4 to 7 of the 200 standard files, depending on the operation, and nupdf one (which MuPDF also fails). On 7 of the 30 long documents its page-size call fails on boxes the pages inherit.

## Agents

Thirty graded tasks, five runs of each per arm, one small-model agent (Claude Haiku 5.5) per run, headless in Claude Code with the arm's tool only: 1,045 runs ([eval/REPORT.md](../eval/REPORT.md)). The tasks:
- find a sentence and its page, check citations, find a phrase in 352 pages;
- read invoices, forms and a printed claim form, sum a bundle of 150 receipts;
- triage damaged and locked files, open one with a password;
- merge, split at bookmarks, rotate, fill a form, sanitize;
- redact a secret and PII across bundles;
- read comments, links, attachments, signatures, page sizes and scanned pages;
- compare two versions, read two columns in order.

The graders use pdftotext, qpdf and MuPDF, never nupdf.

| Arm | Passed | Median tokens, passed runs | Median wall time |
|---|---|---|---|
| nupdf MCP | 150 / 150 | 8,988 | 13 s |
| nupdf CLI | 150 / 150 | 14,578 | 16 s |
| pymupdf4llm (with PyMuPDF), in Python | 147 / 150 | 12,805 | 23 s |
| pypdf and pdfplumber, in Python | 145 / 150 | 11,823 | 21 s |
| PDF Oxide, in Python | 108 / 150 | 20,858 | 30 s |
| PDF Oxide's MCP server | 79 / 105 | 15,436 | 20 s |
| pymupdf4llm-mcp | 38 / 105 | 4,617 | 23 s |
| the model reading page images | 75 / 85 | 17,402 | 12 s |

The arms that cannot write a PDF skip the tasks that do.

Compared task by task over the tasks both solved:
- **nupdf over MCP spent the least.** The other arms used 1.16x (pymupdf4llm) to 2.14x (PDF Oxide in Python) its tokens, and 1.13x to 1.89x its wall time.
- **The CLI spent slightly more tokens than two arms:** pymupdf4llm used 0.86x its tokens and pypdf 0.95x. It was still faster than both.
- **Tool calls were even**, at a median of seven a run.

## Where nupdf falls short

- **Tokens over the CLI.** An agent using the CLI spends slightly more than one writing PyMuPDF or pypdf scripts. That is the gap to close for agents that do not use MCP.
- **Rendering not yet ported:**
  - stroked text (FreeType's stroker);
  - CJK text in fonts the file does not embed;
  - FreeText annotations and form fields with rich text, which MuPDF lays out with its HTML engine.
- **Slower than MuPDF on a few files.** `sanitize` saves, reparses and verifies its output, so it does more than `mutool clean` and takes longer (24 s against 9 s on the worst file seen). `tables` takes 20 to 52 s on two files from the open web, with MuPDF's own algorithm.
- **One machine, one model.** The speed figures come from a busy Apple M-series Mac, and the agent figures from one model. Five runs per task damp the model's variance (a task's tokens vary by a median 1.6x between its runs) but do not remove it.
- **Not yet public.** Until the repository is public, there is no continuous fuzzing on OSS-Fuzz and no outside review.
