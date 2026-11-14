# Benchmarks

Speed of nupdf against MuPDF, the reference, and PDF Oxide ([oxide.fyi](https://oxide.fyi), the `pdf_oxide` crates). How the runs work is in [TESTING](TESTING.md#benchmarks); earlier runs are [at the end](#earlier-runs). The raw data for this page is in `bench/results/2026-11-13-main-ca67177/` and `bench/results/2026-11-14-calls/`.

## Against MuPDF and PDF Oxide, main at ca67177, 2026-11-13

Four comparisons, each answering its own question:

- **Per command against MuPDF**: every nupdf command that has a MuPDF counterpart, against `mutool`, as processes, start included.
- **Engines**: nupdf's library against PDF Oxide's, Rust to Rust, in one process on bytes already in memory. This is the comparison of the two engines.
- **Per call**: nupdf's CLI against `mutool` and PDF Oxide's CLI, `pdf-oxide`, as processes, start included. This is what an agent with a shell pays for each command.
- **Python**: PDF Oxide's Python bindings, which the agent eval's pdf_oxide arm used, as processes.

```bash
cargo xtask bench --tier standard --sample 200 --runs 3                     # against MuPDF
cargo xtask bench --tier long --runs 3
cargo xtask bench --tier standard --sample 200 --runs 5 --against engines   # builds bench/engines
cargo xtask bench --tier long --runs 5 --against engines
cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78        # the pdf-oxide CLI
cargo xtask bench --tier standard --sample 200 --runs 3 --against pdf_oxide
cargo xtask bench --tier long --runs 3 --against pdf_oxide
```

- **Machine**: Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, on mains power, with other work on the machine (load average about 6 on 18 cores). Each command runs one process at a time, on one thread unless it says otherwise.
- **Versions**: nupdf on `main` at `ca67177`, after the speed fixes. MuPDF 1.28.6, the pinned oracle. PDF Oxide 0.3.78 (MIT OR Apache-2.0) three ways: the `pdf_oxide` crate from crates.io with the features its CLI builds it with (`rendering`, `logging`, and its defaults, ICC through qcms and legacy crypto; not `parallel`); the CLI `pdf-oxide` (crate `pdf_oxide_cli`, built with `cargo install --locked`); and the Python bindings from PyPI on CPython 3.12.11 (`oracle/bench/requirements.txt`).
- **Builds**: the engines crate, `bench/engines`, is outside the workspace (no product crate depends on pdf_oxide) and builds both libraries with the root workspace's release profile (thin LTO, one codegen unit).
- **Files**: `standard` is an evenly spaced 200 of the standard tier's files, mostly small; `long` is the 30 documents of 96 to 362 pages merged from them (3,414 pages).

Ratios are MuPDF's (or PDF Oxide's) time over nupdf's: above 1x, nupdf is faster. Each is a geometric mean over the files both tools did and, where checked, agree on; "totals" is the ratio of the summed times, which the largest files weigh most. Against PDF Oxide, a file both tools did is checked where their outputs must agree: the same page count, both with text or both without, the same number of pages with ink. A file on which they do not is counted as a disagreement and left out of the ratios, so that a fast empty answer is not a win.

### Per command against MuPDF

Each pair runs the same task with `mutool` and with `nupdf`: one warm-up run, then the median of 3. Where MuPDF has no command, a script run by `mutool run` does the task (`oracle/js/`), its start included. nupdf's writing commands read their output back to verify it; MuPDF's do not.

| Command | What it does | Standard | Totals | Long | Totals |
|---|---|---|---|---|---|
| info | open, read the version, count pages and objects (nupdf also scans for scanned pages) | 1.57x | 1.48x | 0.26x | 0.15x |
| clean | rewrite with `clean`'s defaults | 1.38x | 1.41x | 1.02x | 0.82x |
| clean -gggg | rewrite with every garbage collection level | 1.34x | 1.36x | 0.94x | 1.03x |
| clean -z | rewrite, deflating every stream without a filter | 1.24x | 1.16x | 0.35x | 0.15x |
| merge | merge the document with itself: pages, links, outline | 1.37x | 1.38x | 0.85x | 0.81x |
| split | each page to a file of its own (MuPDF: `split.js`) | 1.72x | 1.65x | 1.18x | 0.98x |
| tables | every page's tables, by MuPDF's table hunting | 2.02x | 2.27x | 3.77x | 5.25x |
| annots | every page's annotations | 1.98x | 1.99x | 1.51x | 1.52x |
| forms | every page's widgets | 1.99x | 2.00x | 1.45x | 1.42x |
| signatures | every signature checked, digest included | 1.92x | 1.93x | 1.44x | 1.39x |
| flatten | annotations and fields baked in, the document written | 1.89x | 1.91x | 1.43x | 1.17x |
| stext | structured text of every page as JSON | 1.77x | 2.25x | 3.44x | 6.91x |
| stext, 8 threads | the same, nupdf on 8 threads, MuPDF on one | 1.77x | 2.35x | 6.71x | 17.43x |
| render | every page at 72 dpi as PPM | 1.28x | 1.16x | 1.97x | 29.93x |
| render, 8 threads | the same, nupdf on 8 threads, MuPDF on one (the oracle is built without threads) | 1.32x | 1.32x | 5.20x | 81.87x |

- **Small files**: nupdf is faster on every command, 1.24x (`clean -z`) to 2.02x (`tables`).
- **Long documents**: faster on reading, extraction, tables, rendering and annotations, 1.4x to 3.8x in geometric mean on one thread and up to 6.7x on eight. Slower on two things:
  - `info`, 0.26x: nupdf's scans every page's text for pages that need OCR, which MuPDF's does not. That takes it 5 to 168 ms a document (`long-017.pdf` the most), against 4 to 6 ms for MuPDF. Opening and counting pages alone, nupdf's library is 5.85x faster than PDF Oxide's (below).
  - Writing long documents: `clean -z` 0.35x (0.15x in total: 3.9 s against 0.59 s) and `merge` 0.85x; `clean`, `clean -gggg` and `split` are about even (0.94x to 1.18x, 0.82x to 1.03x in total). The same commands are 1.24x to 1.72x faster on small files, so the cost grows with the document ([#86](https://github.com/raspao-studio/nupdf/issues/86)). `flatten`, which also writes, is 1.43x faster.
- **`render` totals on the long tier** are one document's: `mutool draw` ran into the bench's 300 s timeout on `long-018.pdf` (nupdf: 0.52 s on one thread, 0.18 s on eight), and the pair counts the timeout as MuPDF's time, so 300 of MuPDF's 315 s are that file, and its real time is longer. Without it, `render` is 1.62x in geometric mean and 1.50x in total, `render` on 8 threads 4.26x and 3.81x.
- **Thread scaling** (`stext-scaling`, `extract-scaling`: nupdf on one thread against eight, for documents of 50 pages or more that take 100 ms or more on one): on the long tier, where 6 documents qualify, `stext` is 3.13x faster on eight threads (3.15x in total) and `extract` 2.97x (2.97x); on the standard tier one document qualifies, at 3.21x and 3.08x.

### Engines: nupdf's library against PDF Oxide's

Each run opens the document from the bytes in memory and does the operation on every page; the clock covers only that, not reading the file (nor the copy of the bytes PDF Oxide takes by value). The first run is a warm-up whose output is checked and not timed; the median of the next 5 is reported. Each file, operation and library runs in a child process of its own, so an error, a panic (caught with `catch_unwind`), an abort or a hang is that file's failure only. Outputs are kept alive with `std::hint::black_box`.

| Operation | nupdf | PDF Oxide |
|---|---|---|
| open | `Document::open`, `page_count` | `PdfDocument::from_bytes`, `page_count` |
| pages | each page's `page` and inherited MediaBox and Rotate | `get_page_media_box`, `get_page_rotation` |
| text | `parallel::extract_pages` on one thread (as `nupdf extract`), `StextPage::text`, into one String | `extract_text` of every page (it looks for tables), into one String |
| text-fast | as `text` | `extract_text_with_options` with `extract_tables: false` |
| render | `draw::render_page_with` through `parallel::run_pages`, RGB, ICC on (the CLI's default) | `PageRenderer::render_page`, `RawRgba8` (premultiplied RGBA), ICC through qcms |
| tables | structured text with table hunting and vectors (as `nupdf tables`), the tables counted | `extract_tables` |
| annots | `annot::page_annots`, each one's type, contents, rectangle and author | `get_annotations` |

Both render at 72 dpi into a pixmap in memory, without encoding and without annotations (nupdf's page contents alone, PDF Oxide's `render_annotations: false`), with the same 100-megapixel limit. Annotations are counted without links, popups and widgets on both sides.

Standard tier, 200 files:

| Operation | PDF Oxide / nupdf | Totals | nupdf total ms | PDF Oxide total ms | Failed: nupdf, PDF Oxide | Disagree |
|---|---|---|---|---|---|---|
| open | 2.21x | 1.03x | 9.2 | 9.5 | 1, 4 | 2 |
| pages | 2.49x | 1.53x | 9.7 | 14.9 | 1, 6 | 1 |
| text | 1.40x | 2.22x | 189 | 419 | 1, 4 | 6 |
| text-fast | 1.24x | 1.68x | 155 | 260 | 1, 4 | 6 |
| render | 1.58x | 3.20x | 1,002 | 3,204 | 1, 6 | 4 |
| tables | 1.27x | 1.70x | 195 | 332 | 1, 4 | |
| annots | 2.41x | 1.44x | 9.6 | 13.9 | 1, 5 | |

Long tier, 30 documents of 96 to 362 pages:

| Operation | PDF Oxide / nupdf | Totals | nupdf total ms | PDF Oxide total ms | Failed: nupdf, PDF Oxide | Disagree |
|---|---|---|---|---|---|---|
| open | 5.85x | 7.35x | 2.6 | 19.4 | 0, 0 | 0 |
| pages | 11.45x | 9.94x | 8.5 | 84.4 | 0, 7 | 0 |
| text | 1.82x | 2.57x | 1,623 | 4,167 | 0, 0 | 0 |
| text-fast | 1.55x | 2.01x | 1,611 | 3,234 | 0, 0 | 0 |
| render | 3.08x | 7.36x | 2,580 | 18,992 | 1, 5 | 9 |
| tables | 1.55x | 1.34x | 2,047 | 2,744 | 0, 0 | |
| annots | 22.03x | 8.10x | 33.4 | 271 | 0, 0 | |

- nupdf's library is faster on every operation and both tiers, in geometric mean and in total: 1.2x to 3.1x for text, rendering and tables, 2.2x to 22x for opening, page-tree walks and annotations.
- The long tier's largest single costs are now PDF Oxide's: it renders `long-008.pdf` in 13.6 s (nupdf 0.76 s) and `long-013.pdf` in 5.8 s (nupdf 0.54 s). The documents that used to decide nupdf's totals take it 0.47 s to render (`long-018.pdf`), 0.34 s for tables (`long-012.pdf`, where nupdf and MuPDF find 16 tables and PDF Oxide 1), and 26 ms for text (`long-006.pdf`).
- **Failures.** nupdf fails `crypt_filters.pdf` (a crypt filter it does not support, as MuPDF does not), which PDF Oxide reads. It also fails to render one long document, `long-014.pdf`, whose 12,608 by 16,806 pt page is over the 100-megapixel limit at 72 dpi; PDF Oxide scales such a page down to fit its limit instead. PDF Oxide fails 4 to 6 standard files. Three are damaged files that nupdf and MuPDF repair (`repeat_viewer_ref.pdf`, `bug1020226.pdf`, `issue-119.pdf`). One is `enc-XI-R6,V5,U=wwwww,O=wwwww.pdf`, which needs a password: nupdf opens it and counts 0 pages, PDF Oxide returns an error, so in effect both refuse it. The rest are page boxes it cannot find and a few decoding errors. On the long tier it fails 7 page-tree walks (a page that inherits its MediaBox: `get_page_media_box` does not look up the tree) and 5 renders (unsupported or damaged streams).
- **Disagreements** in rendering are pages one library leaves blank and the other does not, 1 to 8 pages a document.
- **Warm against cold.** The warm-up absorbs one-time work. In PDF Oxide's case it can be large: `long-015.pdf`'s text takes 2.2 s on the first call in a process and 59 ms after that. The per-call figures below include it; these do not.

### Per call: nupdf, MuPDF and the `pdf-oxide` CLI

The CLI is what an agent with a shell would run. Where it has a command for the operation, this is the per-call comparison: wall time of the whole process, start included, median of 3 runs after one that must succeed. A side fails a file when it exits with an error or runs over 300 s; `mutool` exits 1 when any page has an error, even when it writes every page.

| Operation | nupdf | MuPDF | `pdf-oxide` CLI |
|---|---|---|---|
| info | `info`: version, page count, objects, permissions, metadata, and every page's text scanned for pages that need OCR | `info.js`: the same without the scan | `info --json`: version, page count, metadata |
| text | `extract --format text` to stdout, annotations' text included | `draw -F txt` to a file | `text` to stdout |
| render-png | `render` (PNG), with annotations | `draw -N`, PNG, with annotations | `render -o dir`, RGBA PNG, with annotations |
| forms | `forms`: each page's widgets | `forms.js` | `forms --json`: the AcroForm's fields |

| Operation | Tier | MuPDF / nupdf | CLI / nupdf | CLI totals | Failed: MuPDF, nupdf, CLI | Disagree |
|---|---|---|---|---|---|---|
| info | standard | 1.64x | 1.11x | 1.01x | 1, 1, 4 | 2 |
| text | standard | 1.68x | 1.26x | 1.31x | 10, 1, 4 | 4 |
| render-png | standard | 1.28x | 2.16x | 1.93x | 10, 1, 7 | 8 |
| forms | standard | 2.04x | 1.33x | 1.33x | 1, 1, 4 | |
| info | long | 0.24x | 0.24x | 0.16x | 0, 0, 0 | 0 |
| text | long | 1.96x | 1.86x | 3.68x | 10, 0, 0 | 0 |
| render-png | long | 1.48x | 0.73x | 3.06x | 10, 0, 5 | 10 |
| forms | long | 1.48x | 0.91x | 0.65x | 0, 0, 0 | |

- On small files both CLIs are native processes that start in a few milliseconds, and nupdf is 1.1x to 2.2x faster per call.
- **Text** on the long tier: nupdf is 1.86x faster in geometric mean and 3.68x in total (1.4 s against 5.2 s). The CLI's text is the same as the library's and the Python bindings' to the character.
- **info** on the long tier is nupdf's loss: `nupdf info` scans every page's text for pages that need OCR (up to 0.16 s on `long-017.pdf`, against about 3 ms for the CLI and 4 ms for MuPDF). Opening and counting pages alone, nupdf's library is 5.85x faster than PDF Oxide's (above).
- **render-png** on the long tier: the CLI is faster on most documents (0.73x in geometric mean) and far slower on a few (3.06x in total: 13.7 s against nupdf's 0.82 s on `long-008.pdf`). nupdf's library draws faster than PDF Oxide's (3.08x above), and nupdf is 1.48x faster than MuPDF, which writes PNGs of the same size; the difference with the CLI is encoding PNG. On `long-020.pdf` nupdf draws 100 pages in 6 ms and takes 171 ms with PNG; the CLI takes 64 ms with PNG. PDF Oxide's PNGs are RGBA and about twice the size (1.7 MB against 0.8 MB for that document, where nupdf and MuPDF write the same size), so it compresses less. Speeding up nupdf's PNG encoder is [#81](https://github.com/raspao-studio/nupdf/issues/81).
- **forms** on the long tier: nupdf walks every page's widgets where the CLI reads the AcroForm; these documents have no fields, and both take about 3 ms in the median.

### PDF Oxide in Python, as the agent eval used it

The same operations through the Python bindings, as processes: the interpreter's start and `import pdf_oxide` take 32 to 35 ms a call (median per operation; 142 ms for the long tier's raw render), which is most of the time on small files. Python's time over nupdf's, per call:

| Operation | standard | long |
|---|---|---|
| info | 12.7x | 1.96x |
| pages | 15.6x | 13.5x |
| text | 12.9x | 4.31x |
| text-fast | 12.9x | 3.94x |
| render (raw pixmap, written as PAM) | 11.4x | 5.82x |
| render-png | 7.79x | 0.90x |
| tables | 12.9x | 3.59x |
| annots | 15.7x | 10.2x |
| forms | 15.6x | 8.76x |
| signatures | 15.7x | 8.82x |

The bench also times Python in process, against nupdf's process time less its start. That comparison is not the engines': nupdf's side still includes reading the file, hashing it for `doc_id` and writing a report, so it is kept in the raw data only, and the engines comparison above replaces it.

### What the outputs show

- **Failures.** The CLI and Python fail mostly the same files as the library, through the same parser, and the CLI's render stops at the first page that fails.
- **Disagreements** in the CLI and Python runs are mostly about what each tool draws or counts. Text: on 1 file (`bug_1206.pdf`) PDF Oxide has text where nupdf has none; on 3 nupdf has text and PDF Oxide none (`bad24.pdf`, a `.notdef` test, and a FreeText annotation's text). Render: on 4 standard files the only ink is an annotation, which nupdf draws and PDF Oxide does not; the other 4 differ in the page contents, as in the engines run. On the long tier the counts of pages with ink differ by 1 to 8 a document. Info and pages: PDF Oxide counts 1 or 2 pages in two files whose page tree has none (MuPDF and nupdf count 0).
- **Counts** that are not checked differ by design: PDF Oxide's tables are found its own way (9 against nupdf's 40 on the standard tier), and its form fields are the AcroForm's terminal fields rather than each page's widgets. With links, popups and widgets left out on both sides the annotation counts are close (40 against 39).
- **Correctness seen in passing.** PDF Oxide's page boxes do not inherit: on `corpus/smoke/stext.pdf`, whose pages inherit a 300 by 300 MediaBox from the page tree, Python's `page_media_box` returns US Letter, and the library's `get_page_media_box` fails outright on 7 long documents. On `long-005.pdf` its text has 61% of nupdf's characters, which the both-have-text check does not catch.

### Caveats

- `info` is not like for like in the per-command and per-call comparisons (nupdf's scans every page's text); the engines' `open` is.
- In the `render` pairs against MuPDF, `mutool draw -N` has colour management off and `nupdf render` has it on, its default.
- A MuPDF pair counts a run over its 300 s timeout as 300 s (`long-018.pdf` above); the per-call comparison counts it as a failure.
- In the engines comparison, nupdf renders with ICC on, its CLI's default; PDF Oxide's ICC is qcms, its default. nupdf has no speed-first text mode, so its `text-fast` row is its `text` run again.
- In the per-call comparison PDF Oxide's PNGs are RGBA. The CLI has no tables, annotations, signatures or page-box commands, and no speed-first text option.
- The machine was not idle: the standard tier's engine totals are about twice the earlier run's (below) on both sides, for the same work, so compare ratios, not milliseconds, across runs. One-off spikes move single files; medians and geometric means damp them but do not remove them.

## One call, and a long document: the text, eleven ways, 2026-11-14

What an agent pays to get a file's text, against every tool it might use instead. Each tool is asked for the text of every page on stdout, as a process of its own: start-up and imports are in the time. The raw data is in `bench/results/2026-11-14-calls/`; the website's two speed tables are these.

```bash
cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78
python3 bench/calls/run.py --set typical --runs 5 --corpus ~/.cache/nupdf/corpus --nupdf target/release/nupdf --mutool MUTOOL --oxide target/oxide-cli/bin/pdf-oxide --python VENV/bin/python --out OUT/typical
python3 bench/calls/run.py --set long --runs 5 --limit 300 ...   --out OUT/long
python3 bench/calls/summary.py OUT/typical --mutool MUTOOL
```

- **Machine**: Apple M5 Max, macOS 27.2.1, with a load average of 2 to 3 for most of the run (about 12 as the `typical` set began). One Apple silicon Mac is the only machine this has run on.
- **Versions**: nupdf 0.1.0 (`main` at `7a964d8`), MuPDF 1.28.6, Poppler 26.04.0, PDF Oxide 0.3.78 (its CLI and its Python bindings), and on Python 3.12.11: PyMuPDF 2.1, pypdfium2 5.15.2 (PDFium 8149), pypdf 7.0.1, pdfplumber 0.11.10, pdfminer.six 20260107.
- **Files**: `typical` is an evenly spaced 200 of the standard tier's 5,142 files; `long` is the 30 long documents.
- **Calls**: `nupdf extract --format text` (and with `--threads 8`), `mutool draw -q -F txt`, `pdftotext FILE -`, `pdf-oxide text`, and for each Python library a one-line script run with `python -c`. Each call runs five times; a call's time is the median. `mutool` exits 1 when a page had an error and still writes the rest, which counts as done.
- **Speed and text are judged apart.** Times are over the files every tool completed (187 of 200, and 17 of 30 with 1,777 pages), so no tool is timed on a set of its own. A file's text is gradable when MuPDF's and PDFium's words for it agree (token F1 of 0.9 or more, through their Python bindings), and a call's text is right when its words match PDFium's (F1 of 0.8 or more). PDFium is the judge because nupdf follows MuPDF.

**One call** (`typical`):

| Tool | Median call | Fastest of 5, median | Processor time, median | Peak memory, median | Done | Right text |
|---|---|---|---|---|---|---|
| nupdf | 10.8 ms | 6.2 ms | 0 ms | 6.5 MB | 199 / 200 | 162 / 162 |
| PDF Oxide, `pdf-oxide` CLI | 11.9 ms | 10.7 ms | 0 ms | 10.8 MB | 196 / 200 | 157 / 162 |
| MuPDF, `mutool` | 11.9 ms | 10.7 ms | 0 ms | 5.6 MB | 191 / 200 | 161 / 162 |
| Poppler, `pdftotext` | 23.4 ms | 21.1 ms | 0 ms | 10.4 MB | 194 / 200 | 160 / 162 |
| PDF Oxide, Python bindings | 46.3 ms | 42.4 ms | 20 ms | 30.3 MB | 196 / 200 | 157 / 162 |
| pypdf | 83.8 ms | 78.6 ms | 50 ms | 37.7 MB | 190 / 200 | 156 / 162 |
| PDFium, through pypdfium2 | 84.2 ms | 79.1 ms | 40 ms | 40.5 MB | 194 / 200 | 162 / 162 |
| pdfminer.six | 84.6 ms | 78.8 ms | 50 ms | 35.6 MB | 192 / 200 | 151 / 162 |
| MuPDF, through PyMuPDF | 84.7 ms | 79.3 ms | 50 ms | 55.6 MB | 197 / 200 | 162 / 162 |
| pdfplumber | 84.8 ms | 78.9 ms | 50 ms | 36.3 MB | 192 / 200 | 155 / 162 |

**A long document** (`long`):

| Tool | Pages a second | Total | Processor time, total | Peak memory, median | Peak memory, most | Done | Right text |
|---|---|---|---|---|---|---|---|
| nupdf, 8 threads | 2,729 | 0.65 s | 0.95 s | 32.0 MB | 203 MB | 30 / 30 | 14 / 14 |
| nupdf | 1,568 | 1.13 s | 0.63 s | 13.0 MB | 158 MB | 30 / 30 | 14 / 14 |
| Poppler, `pdftotext` | 1,072 | 1.66 s | 1.16 s | 13.5 MB | 76 MB | 30 / 30 | 13 / 14 |
| PDFium, through pypdfium2 | 757 | 2.35 s | 1.68 s | 52.1 MB | 237 MB | 30 / 30 | 14 / 14 |
| MuPDF, `mutool` | 498 | 3.57 s | 2.97 s | 10.0 MB | 83 MB | 30 / 30 | 14 / 14 |
| PDF Oxide, `pdf-oxide` CLI | 342 | 5.20 s | 4.70 s | 29.1 MB | 230 MB | 30 / 30 | 14 / 14 |
| PDF Oxide, Python bindings | 318 | 5.60 s | 4.82 s | 47.5 MB | 255 MB | 30 / 30 | 14 / 14 |
| MuPDF, through PyMuPDF | 305 | 5.82 s | 5.11 s | 59.3 MB | 109 MB | 30 / 30 | 14 / 14 |
| pypdf | 188 | 9.43 s | 8.78 s | 44.7 MB | 195 MB | 21 / 30 | 10 / 14 |
| pdfminer.six | 105 | 16.94 s | 16.22 s | 42.6 MB | 142 MB | 23 / 30 | 11 / 14 |
| pdfplumber | 69 | 25.68 s | 24.98 s | 84.1 MB | 622 MB | 20 / 30 | 3 / 14 |

- **One call**: the three native tools are within about 10% of each other, which is within what a rerun moves them by. Anything started from Python takes about 85 ms, most of it the interpreter and the import.
- **Long documents**: nupdf leads on one thread and on eight. It is not quicker on every document: `pdftotext` was quicker on 8 of the 30, `pdf-oxide` on 4 and `mutool` on 2.
- **Memory**: `mutool` uses the least in both sets.
- **PDF Oxide's two rows** differ by start-up alone: about 12 ms a call from its CLI against 46 ms from Python, and nearly the same pages a second on long documents.

### Caveats

- Wall times moved between reruns on this machine: an earlier pass gave nupdf 4.9 ms and `mutool` 6.0 ms a call on the same files. The order and the ratios held; the milliseconds are approximate.
- Three Python libraries fail 7 to 10 of the long documents. Some of pypdf's failures are a missing optional dependency here (Brotli), not pypdf's own limit.
- "Right text" compares words, not order or layout.
- The harness must not hold the tools' output: an earlier version did, grew, and every later spawn then cost 180 to 280 ms. `run.py` sends each tool's output straight to a file.

## Earlier runs

Each with the code of its date. Ratios are MuPDF's or PDF Oxide's time over nupdf's, as above.

- **Against PDF Oxide, 2026-11-13, at `1abb7e5`, before the speed fixes** (`bench/results/2026-11-13-pdf_oxide/`, the same commands, load average 7 to 18). In geometric mean nupdf's library was already faster on every operation (engines: 1.10x to 15.4x), but on the long tier it lost the totals of text (0.73x), rendering (0.19x) and tables (0.18x) to three documents: it rendered `long-018.pdf` in 77 s (PDF Oxide 1.7 s), hunted tables in `long-012.pdf` for 11.3 s (PDF Oxide 0.22 s) and spent about 2.5 s on one page of `long-006.pdf`. Per call, `info` on the long tier was 0.17x MuPDF's (up to 2.4 s a document for its scan) and `render-png` 0.84x MuPDF's and 0.29x the CLI's.
- **`clean`, `merge`, `split`, 2026-11-10**, on 300 standard-tier files: `nupdf clean` 614 ms in total against `mutool clean`'s 900 ms (geometric mean 1.50x), its read-back check included; `clean -gggg` 724 ms against 1,035 ms (1.48x); `clean -z` 916 ms against 1,050 ms (1.36x), where the check decodes every stream. `nupdf merge` of each file with itself 677 ms against `mutool merge`'s 924 ms (1.44x), links and outline included; `nupdf split` 935 ms against 1,518 ms for a MuPDF script that does the same (`oracle/js/split.js`; 1.94x, the script's overhead included). `clean` is slower on files of a megabyte or more, where hashing the input and the output for their `doc_id` costs more than MuPDF's whole save; `merge` and `split` on 5 to 11 files, the larger ones, for the same reason.
- **`render`, 2026-11-11**, on 200 standard-tier files at 72 dpi against `mutool draw -N`: one thread, geometric mean 1.25x (1,293 ms in total against 1,357 ms); eight threads, 1.28x (the oracle is built without threads, so MuPDF runs on one).
- **Thread scaling, 2026-11-12** (`stext-scaling`, `extract-scaling`), on 6 performance and 12 efficiency cores with Finder, fseventsd and Spotlight each holding a core: on the standard tier `stext` 3.4x (one document qualifies), `extract` 2.8x (two); on the `long` tier (9 qualify) `stext` 2.36x in geometric mean and 1.29x in total time, `extract` 2.33x and 1.25x. The total was one document's: a single page of `long-006.pdf` took 2.3 s (MuPDF 2.0 s), which threads over pages cannot split. That page has since dropped to 0.02 s, and the run above has the scaling after the fix.
