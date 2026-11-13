# Benchmarks

Speed of nupdf against MuPDF, the reference, and PDF Oxide ([oxide.fyi](https://oxide.fyi), the `pdf_oxide` crates). How the runs work is in [TESTING](TESTING.md#benchmarks); the milestone measures against MuPDF alone (render, text, scaling, `clean`, `merge`, `split`) are in [PLAN](PLAN.md) beside each milestone's exit. The raw data for this page is in `bench/results/2026-11-13-pdf_oxide/`.

## Against MuPDF and PDF Oxide, 2026-11-13

Three comparisons, each answering its own question:

- **Engines**: nupdf's library against PDF Oxide's, Rust to Rust, in one process on bytes already in memory. This is the comparison of the two engines.
- **Per call**: nupdf's CLI against `mutool` and PDF Oxide's CLI, `pdf-oxide`, as processes, start included. This is what an agent with a shell pays for each command.
- **Python**: PDF Oxide's Python bindings, which the agent eval's pdf_oxide arm used, as processes.

```bash
cargo xtask bench --tier standard --sample 200 --runs 5 --against engines   # builds bench/engines
cargo xtask bench --tier long --runs 5 --against engines
cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78        # the pdf-oxide CLI
cargo xtask bench --tier standard --sample 200 --runs 3 --against pdf_oxide
cargo xtask bench --tier long --runs 3 --against pdf_oxide
```

- **Machine**: Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, on mains power, with other work on the machine (load average 7 to 18 on 18 cores). Everything runs on one thread, one process at a time.
- **Versions**: nupdf at `1abb7e5` (this branch changes only the bench). MuPDF 1.28.6, the pinned oracle. PDF Oxide 0.3.78 (MIT OR Apache-2.0) three ways: the `pdf_oxide` crate from crates.io with the features its CLI builds it with (`rendering`, `logging`, and its defaults, ICC through qcms and legacy crypto; not `parallel`); the CLI `pdf-oxide` (crate `pdf_oxide_cli`, built with `cargo install --locked`); and the Python bindings from PyPI on CPython 3.12.11 (`oracle/bench/requirements.txt`).
- **Builds**: the engines crate, `bench/engines`, is outside the workspace (no product crate depends on pdf_oxide) and builds both libraries with the root workspace's release profile (thin LTO, one codegen unit).
- **Files**: `standard` is an evenly spaced 200 of the standard tier's files, mostly small; `long` is the 30 documents of 96 to 362 pages merged from them (3,414 pages).

Ratios are PDF Oxide's (or MuPDF's) time over nupdf's: above 1x, nupdf is faster. Each is a geometric mean over the files both tools did and, where checked, agree on; "totals" is the ratio of the summed times, which the largest files weigh most. A file both tools did is checked where their outputs must agree: the same page count, both with text or both without, the same number of pages with ink. A file on which they do not is counted as a disagreement and left out of the ratios, so that a fast empty answer is not a win.

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

Both render at 72 dpi into a pixmap in memory, without encoding and without annotations (`render_annotations: false`), with the same 100-megapixel limit. Annotations are counted without links, popups and widgets on both sides.

Standard tier, 200 files:

| Operation | PDF Oxide / nupdf | Totals | nupdf total ms | PDF Oxide total ms | Failed: nupdf, PDF Oxide | Disagree |
|---|---|---|---|---|---|---|
| open | 2.14x | 1.03x | 5.1 | 5.3 | 1, 4 | 2 |
| pages | 2.22x | 1.33x | 6.0 | 8.0 | 1, 6 | 1 |
| text | 1.30x | 1.98x | 86 | 171 | 1, 4 | 6 |
| text-fast | 1.15x | 1.61x | 85 | 136 | 1, 4 | 6 |
| render | 1.57x | 3.37x | 530 | 1,786 | 1, 6 | 4 |
| tables | 1.25x | 1.68x | 130 | 218 | 1, 4 | |
| annots | 2.19x | 1.38x | 7.0 | 9.7 | 1, 5 | |

Long tier, 30 documents of 96 to 362 pages:

| Operation | PDF Oxide / nupdf | Totals | nupdf total ms | PDF Oxide total ms | Failed: nupdf, PDF Oxide | Disagree |
|---|---|---|---|---|---|---|
| open | 4.76x | 6.86x | 1.8 | 12.1 | 0, 0 | 0 |
| pages | 7.77x | 6.00x | 8.9 | 53.6 | 0, 7 | 0 |
| text | 1.42x | 0.73x | 3,805 | 2,794 | 0, 0 | 0 |
| text-fast | 1.22x | 0.62x | 3,936 | 2,436 | 0, 0 | 0 |
| render | 1.61x | 0.19x | 82,243 | 15,772 | 1, 5 | 9 |
| tables | 1.10x | 0.18x | 15,303 | 2,803 | 0, 0 | |
| annots | 15.4x | 5.56x | 47.5 | 264 | 0, 0 | |

- In geometric mean nupdf's library is faster on every operation and both tiers: 1.1x to 1.6x for text, rendering and tables, 2x to 15x for opening, page-tree walks and annotations.
- The long tier's totals go the other way for text, rendering and tables, because a few documents take nupdf far longer than its median, and they decide the totals:
  - `long-018.pdf`: nupdf renders it in 77 s, PDF Oxide in 1.7 s. On its own it is most of nupdf's render total.
  - `long-012.pdf`: nupdf's table hunting takes 11.3 s (MuPDF's 6.1 s as a process), PDF Oxide 0.22 s, finding 1 table where nupdf and MuPDF find 16.
  - `long-006.pdf`: one page takes nupdf about 2.5 s for text and tables (MuPDF 2 to 3 s as a process); PDF Oxide reads the whole document's text in 30 ms, with about as many characters (7,397 against 6,992).

  These are nupdf's (and mostly MuPDF's) slow paths, and the ones to profile.
- **Failures.** nupdf fails `crypt_filters.pdf` (a crypt filter it does not support, as MuPDF does not), which PDF Oxide reads. It also fails to render one long document, `long-014.pdf`, whose 12,608 by 16,806 pt page is over the 100-megapixel limit at 72 dpi; PDF Oxide scales such a page down to fit its limit instead. PDF Oxide fails 4 to 6 standard files. Three are damaged files that nupdf and MuPDF repair (`repeat_viewer_ref.pdf`, `bug1020226.pdf`, `issue-119.pdf`). One is `enc-XI-R6,V5,U=wwwww,O=wwwww.pdf`, which needs a password: nupdf opens it and counts 0 pages, PDF Oxide returns an error, so in effect both refuse it. The rest are page boxes it cannot find and a few decoding errors. On the long tier it fails 7 page-tree walks (a page that inherits its MediaBox: `get_page_media_box` does not look up the tree) and 5 renders (unsupported or damaged streams).
- **Disagreements** in rendering are pages one library leaves blank and the other does not, 1 to 8 pages a document.
- **Warm against cold.** The warm-up absorbs one-time work. In PDF Oxide's case it can be large: `long-015.pdf`'s text takes 2.3 s on the first call in a process and 50 ms after that. The per-call figures below include it; these do not.

### Per call: nupdf, MuPDF and the `pdf-oxide` CLI

The CLI is what an agent with a shell would run. Where it has a command for the operation, this is the per-call comparison: wall time of the whole process, start included, median of 3 runs after one that must succeed. A side fails a file when it exits with an error or runs over 300 s; `mutool` exits 1 when any page has an error, even when it writes every page.

| Operation | nupdf | MuPDF | `pdf-oxide` CLI |
|---|---|---|---|
| info | `info`: version, page count, objects, permissions, metadata, and every page's text scanned for pages that need OCR | `info.js`: the same without the scan | `info --json`: version, page count, metadata |
| text | `extract --format text` to stdout | `draw -F txt` to a file | `text` to stdout |
| render-png | `render` (PNG), without annotations | `draw -N`, PNG | `render -o dir`, RGBA PNG, with annotations |
| forms | `forms`: each page's widgets | `forms.js` | `forms --json`: the AcroForm's fields |

| Operation | Tier | MuPDF / nupdf | CLI / nupdf | CLI totals | Failed: MuPDF, nupdf, CLI | Disagree |
|---|---|---|---|---|---|---|
| info | standard | 1.54x | 1.15x | 1.03x | 1, 1, 4 | 2 |
| text | standard | 1.63x | 1.31x | 1.35x | 10, 1, 4 | 6 |
| render-png | standard | 1.07x | 1.88x | 1.61x | 10, 1, 7 | 15 |
| forms | standard | 2.07x | 1.34x | 1.33x | 1, 1, 4 | |
| info | long | 0.17x | 0.18x | 0.05x | 0, 0, 0 | 0 |
| text | long | 1.52x | 1.57x | 1.37x | 10, 0, 0 | 0 |
| render-png | long | 0.84x | 0.29x | 0.31x | 10, 0, 5 | 10 |
| forms | long | 1.40x | 0.95x | 0.71x | 0, 0, 0 | |

- On small files both CLIs are native processes that start in a few milliseconds, and nupdf is 1.15x to 1.9x faster per call.
- **Text** on the long tier: nupdf is 1.57x faster in geometric mean and 1.37x in total. The CLI's text is the same as the library's and the Python bindings' to the character.
- **info** on the long tier is nupdf's loss: `nupdf info` scans every page's text for pages that need OCR (2.4 s on `long-006.pdf`, against 5 ms for MuPDF and the CLI). Opening and counting pages alone, nupdf's library is 4.8x faster than PDF Oxide's (above).
- **render-png** on the long tier is nupdf's loss to the CLI (0.29x) and to MuPDF (0.84x), though nupdf's library draws faster than PDF Oxide's (1.61x above) and nupdf's CLI writes PPM 1.49x faster than MuPDF does. The difference is encoding PNG. On `long-020.pdf` nupdf draws 100 pages in 6 ms and takes 367 ms with PNG; the CLI takes 73 ms with PNG. PDF Oxide's PNGs are RGBA and about twice the size (1.7 MB against 0.8 MB for that document, where nupdf and MuPDF write the same size), so it compresses less. nupdf's encoder is also slower than MuPDF's for the same output.
- **forms** on the long tier: nupdf walks every page's widgets where the CLI reads the AcroForm; these documents have no fields, and both take about 5 ms.

### PDF Oxide in Python, as the agent eval used it

The same operations through the Python bindings, as processes: the interpreter's start and `import pdf_oxide` take 33 to 49 ms a call (median per operation), which is most of the time on small files. Python's time over nupdf's, per call:

| Operation | standard | long |
|---|---|---|
| info | 12.6x | 1.32x |
| pages | 15.1x | 12.8x |
| text | 11.9x | 3.63x |
| text-fast | 13.2x | 3.54x |
| render (raw pixmap, written as PAM) | 11.2x | 3.06x |
| render-png | 6.32x | 0.37x |
| tables | 12.6x | 2.80x |
| annots | 15.9x | 9.20x |
| forms | 15.7x | 7.97x |
| signatures | 15.7x | 8.35x |

The bench also times Python in process, against nupdf's process time less its start. That comparison is not the engines': nupdf's side still includes reading the file, hashing it for `doc_id` and writing a report, so it is kept in the raw data only, and the engines comparison above replaces it.

### What the outputs show

- **Failures.** The CLI and Python fail mostly the same files as the library, through the same parser, and the CLI's render stops at the first page that fails.
- **Disagreements** in the CLI and Python runs are mostly about what each tool counts. Text: on 4 files PDF Oxide has text where nupdf has none, each a file with annotations whose appearance text nupdf's `extract` leaves out (`mutool draw -F txt` draws annotations, so MuPDF has it too); on 2 (`bad24.pdf`, a `.notdef` test) nupdf has text and PDF Oxide none. Render: PDF Oxide's CLI and Python draw annotations, nupdf's `render` does not, so on 12 of 15 standard files PDF Oxide has ink where nupdf has none. Info and pages: PDF Oxide counts 1 or 2 pages in two files whose page tree has none (MuPDF and nupdf count 0).
- **Counts** that are not checked differ by design: PDF Oxide's tables are found its own way (9 against nupdf's 40 on the standard tier), and its form fields are the AcroForm's terminal fields rather than each page's widgets. With links, popups and widgets left out on both sides the annotation counts are close (40 against 39).
- **Correctness seen in passing.** PDF Oxide's page boxes do not inherit: on `corpus/smoke/stext.pdf`, whose pages inherit a 300 by 300 MediaBox from the page tree, Python's `page_media_box` returns US Letter, and the library's `get_page_media_box` fails outright on 7 long documents. On `long-005.pdf` its text has 61% of nupdf's characters, which the both-have-text check does not catch.

### Caveats

- `info` is not like for like in the per-call comparison (nupdf's scans every page's text); the engines' `open` is.
- In the engines comparison, nupdf renders with ICC on, its CLI's default; PDF Oxide's ICC is qcms, its default. nupdf has no speed-first text mode, so its `text-fast` row is its `text` run again.
- In the per-call comparison, MuPDF and PDF Oxide draw annotations and nupdf does not, and PDF Oxide's PNGs are RGBA. The CLI has no tables, annotations, signatures or page-box commands, and no speed-first text option.
- The per-call `text` rows come from a rerun of that operation alone, after the CLI's page separators (`---` lines) were left out of its character count.
- The machine was not idle. One-off spikes move single files; medians and geometric means damp them but do not remove them.
