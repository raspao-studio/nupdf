# Benchmarks

Speed of nupdf against MuPDF, the reference, and PDF Oxide ([oxide.fyi](https://oxide.fyi), the `pdf_oxide` crates). How the runs work is in [TESTING](TESTING.md#benchmarks); the milestone measures against MuPDF alone (render, text, scaling, `clean`, `merge`, `split`) are in [PLAN](PLAN.md) beside each milestone's exit. The raw data for this page is in `bench/results/2026-11-13-pdf_oxide/`.

## Against MuPDF and PDF Oxide, 2026-11-13

```bash
cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78   # the pdf-oxide CLI
cargo xtask bench --tier standard --sample 200 --runs 3 --against pdf_oxide
cargo xtask bench --tier long --runs 3 --against pdf_oxide
```

- **Machine**: Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, on mains power, with other work on the machine (load average 7 to 18 on 18 cores). Every process runs on one thread, one at a time.
- **Versions**: nupdf at `1abb7e5` (release build; this branch changes only the bench), MuPDF 1.28.6 (the pinned oracle), PDF Oxide 0.3.78 both ways: the native CLI `pdf-oxide` (crate `pdf_oxide_cli`, MIT OR Apache-2.0, built with `cargo install --locked`) and the Python bindings from PyPI on CPython 3.12.11 (`oracle/bench/requirements.txt`), which the agent eval's pdf_oxide arm used.
- **Files**: `standard` is an evenly spaced 200 of the standard tier's files, mostly small (nupdf's median process time is 2 to 4 ms, mostly its start); `long` is the 30 documents of 96 to 362 pages merged from them (3,414 pages).
- **Medians** of 3 timed runs after a first run that must succeed.

What each side runs and writes:

| Operation | nupdf | MuPDF | `pdf-oxide` CLI | PDF Oxide in Python |
|---|---|---|---|---|
| info | `info`: JSON with version, page count, objects, permissions, metadata, and every page's text scanned for pages that need OCR | `info.js`: the same without the scan | `info --json`: version, page count, metadata | `PdfDocument`, `version()`, `page_count` |
| pages | `pages`: each page's object and inherited MediaBox, CropBox, Rotate, Resources | `pages.js`: the same | none | `page_media_box`, `page_crop_box`, `page_rotation` of every page, as JSON |
| text | `extract --format text` to stdout | `draw -F txt` to a file | `text` to stdout | `extract_text(i)` of every page, to a file; `text-fast` with `extract_tables=False` |
| render | `render --format ppm`, without annotations | `draw -N`, PPM, with annotations | none (PNG and JPEG only) | `render_pixmap(i)`, written as PAM (RGBA), with annotations |
| render-png | `render` (PNG), without annotations | `draw -N`, PNG | `render -o dir`, RGBA PNG | `render_page(i)`, RGBA PNG |
| tables | `tables`: JSON rows of cells | `tables.js` (table hunting) | none | `extract_tables(i)` as JSON |
| annots | `annots` (widgets and links left out) | `annots.js` | none | `get_annotations(i)` (widgets and links included) as JSON |
| forms | `forms`: each page's widgets | `forms.js` | `forms --json`: the AcroForm's fields | `get_form_fields()` as JSON |
| signatures | `signatures`: byte ranges, digest | `signatures.js` | none | `signatures()`, each `verify_detached` |

Rendering is at 72 dpi throughout.

Ratios are geometric means of each tool's time over nupdf's: above 1x, nupdf is faster. "Per call" is wall time of the whole process, start included, which is what an agent pays for each command it runs. "In process" leaves the start out on both sides, for the Python bindings only: PDF Oxide's own clock from opening the document to the last byte written (no interpreter, no `import`), and nupdf's process time less the fastest of 21 runs of `nupdf --version` (2.2 ms in the standard run, 2.1 ms in the long one), taken as at least 0.5 ms, both of which err against nupdf. "Totals" is the ratio of summed times, which large files weigh more.

A file counts in a ratio when both tools it compares did it: exit 0 and, in Python, no page raised. For info, pages, text and the renders, nupdf and PDF Oxide must also agree (the same page count; both with text or both without; the same number of pages with ink, read from the files each wrote), so a fast empty answer is not a win. `mutool` exits 1 when any page has an error, even when it writes every page, which counts as a failure, as does any run over 300 s.

### Per call: nupdf, MuPDF and the `pdf-oxide` CLI

The CLI is what an agent with a shell would run, and where it has a command for the operation it is the per-call comparison.

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

- On small files the CLI and nupdf are close: both are native processes that start in a few milliseconds, and nupdf is 1.15x to 1.9x faster per call.
- **Text** on the long tier: nupdf is 1.57x faster in geometric mean and 1.37x in total. The CLI's text is the same as the Python bindings' to the character, so the checks that hold for one hold for the other.
- **info** on the long tier is nupdf's loss to both tools: `nupdf info` scans every page's text for pages that need OCR (2.4 s on `long-006.pdf`, against 5 ms for MuPDF and the CLI).
- **render-png** on the long tier is nupdf's loss to the CLI (0.29x) and to MuPDF (0.84x), where with PPM output nupdf is 1.49x faster than MuPDF. The difference is encoding PNG. PDF Oxide's PNGs are RGBA and about twice the size (1.7 MB against 0.8 MB for `long-020.pdf`'s 100 pages, where nupdf and MuPDF write the same size), so it compresses less and spends less time on it. nupdf's encoder is slower than MuPDF's for the same output. Two documents add to it: `long-018.pdf` takes nupdf 56 s and the CLI 1.9 s (MuPDF was still drawing it after 10 minutes), and `long-002.pdf` 2.6 s against 0.6 s.
- **forms** on the long tier: nupdf walks every page's widgets where the CLI reads the AcroForm; these documents have no fields, so the CLI has nothing to do and both take about 5 ms.

### PDF Oxide in Python, as the agent eval used it

The same operations through the Python bindings. Per call, the interpreter's start and `import pdf_oxide` (33 to 49 ms, the median per operation) make Python 6x to 16x slower than nupdf on small files, so the in-process columns are the ones that compare the engines.

Standard tier, 200 files:

| Operation | MuPDF / nupdf | Python / nupdf, per call | in process | in-process totals | Failed: MuPDF, nupdf, Python | Disagree |
|---|---|---|---|---|---|---|
| info | 1.54x | 12.6x | 0.17x | 0.12x | 1, 1, 4 | 2 |
| pages | 1.94x | 15.1x | 0.45x | 0.42x | 1, 1, 5 | 1 |
| text | 1.63x | 11.9x | 0.48x | 0.74x | 10, 1, 4 | 6 |
| text-fast | 1.76x | 13.2x | 0.74x | 0.93x | 10, 1, 4 | 6 |
| render | 1.34x | 11.2x | 4.94x | 3.93x | 10, 1, 7 | 15 |
| render-png | 1.07x | 6.32x | 2.05x | 1.58x | 10, 1, 7 | 15 |
| tables | 2.08x | 12.6x | 0.60x | 0.90x | 1, 1, 4 | |
| annots | 2.07x | 15.9x | 0.44x | 0.45x | 1, 1, 5 | |
| forms | 2.07x | 15.7x | 0.61x | 0.61x | 1, 1, 4 | |
| signatures | 2.02x | 15.7x | 0.67x | 0.63x | 1, 1, 4 | |

Long tier, 30 documents of 96 to 362 pages:

| Operation | MuPDF / nupdf | Python / nupdf, per call | in process | in-process totals | Failed: MuPDF, nupdf, Python | Disagree |
|---|---|---|---|---|---|---|
| info | 0.17x | 1.32x | 0.02x | 0.01x | 0, 0, 0 | 0 |
| pages | 1.54x | 12.8x | 8.05x | 7.38x | 0, 0, 0 | 0 |
| text | 1.52x | 3.63x | 1.67x | 1.41x | 10, 0, 0 | 0 |
| text-fast | 1.50x | 3.54x | 1.43x | 1.35x | 10, 0, 0 | 0 |
| render | 1.49x | 3.06x | 1.45x | 0.27x | 10, 0, 5 | 10 |
| render-png | 0.84x | 0.37x | 0.28x | 0.39x | 10, 0, 5 | 10 |
| tables | 2.94x | 2.80x | 1.13x | 0.35x | 0, 0, 0 | |
| annots | 1.39x | 9.20x | 1.44x | 2.08x | 0, 0, 0 | |
| forms | 1.40x | 7.97x | 0.33x | 0.23x | 0, 0, 0 | |
| signatures | 1.37x | 8.35x | 0.43x | 0.34x | 0, 0, 0 | |

- On the standard tier's small files PDF Oxide's work is a fraction of a millisecond, and what is left of nupdf's process after its start includes hashing the whole file for `doc_id` and writing a versioned report, so in process PDF Oxide is ahead on everything but rendering.
- On the long tier, where the work outweighs the start, nupdf is ahead in process on text (1.67x, 1.43x against the speed-first mode), raw rendering (1.45x), tables (1.13x), page-tree walks (8.1x; PDF Oxide's per-page box calls) and annotations (1.44x). Totals are swayed by single documents both ways:
  - `long-018.pdf` and `long-002.pdf` (above) make raw rendering's total 0.27x.
  - `long-012.pdf`: nupdf's `tables` takes 11.7 s, MuPDF's table hunting 6.1 s, PDF Oxide 0.25 s (finding 1 table where nupdf and MuPDF find 16).
  - `long-006.pdf`: one page takes nupdf and MuPDF 2 to 3 s for text and tables; PDF Oxide reads the whole document's text in 41 ms, with about as many characters (7,397 against 6,992).
  - The other way, `long-015.pdf` takes PDF Oxide 2.4 s for text and nupdf 93 ms.
- PDF Oxide fails to render 5 of the 30 long documents (a page raises), and disagrees on which pages have ink on 10 more, by 1 to 8 pages each, so the render ratios are over 15 documents.

### What the outputs show

- **Failures.** nupdf fails one standard file (`crypt_filters.pdf`, which MuPDF fails too). PDF Oxide, the CLI and Python alike, fails 4 to 7 depending on the operation: always `repeat_viewer_ref.pdf`, `bug1020226.pdf` and `issue-119.pdf` (damaged files MuPDF and nupdf read) and `enc-XI-R6,V5,U=wwwww,O=wwwww.pdf` (AES-256 with a user password; nupdf and MuPDF report that it needs one), and for some operations `bug_552046.pdf`, `bad-data-out.pdf` and `crypt_filters.pdf`.
- **Disagreements** are mostly about what each tool counts. Text: on 4 files PDF Oxide has text where nupdf has none, each a file with annotations, whose appearance text nupdf's `extract` leaves out (`mutool draw -F txt` draws annotations, so MuPDF has it too); on 2 (`bad24.pdf`, a `.notdef` test) nupdf has text and PDF Oxide none. Render: on 12 of 15, PDF Oxide has ink where nupdf has none, almost all on pages with annotations, which PDF Oxide and MuPDF draw and nupdf's `render` does not; on 3, nupdf has ink where PDF Oxide's page is blank. Info and pages: PDF Oxide counts 1 or 2 pages in two files whose page tree has none (MuPDF and nupdf count 0).
- **Counts** that are not checked differ by design: PDF Oxide's annotations include widgets and links (93 against nupdf's 40 on the standard tier; 33,187 links against 0 on the long tier), its tables are found its own way (9 against 40), and its form fields are the AcroForm's terminal fields rather than each page's widgets.
- **Correctness seen in passing.** PDF Oxide's `page_media_box` does not inherit: on `corpus/smoke/stext.pdf`, whose pages inherit a 300 by 300 MediaBox from the page tree, it returns US Letter. On `long-005.pdf` its text has 61% of nupdf's characters (the same from the CLI), which the both-have-text check does not catch.

### Caveats

- The standard tier is small files, so per-call times there mostly measure starting a process, and the in-process figures for nupdf are a subtraction of two numbers of a few milliseconds. The long tier and the totals are the better measure of the engines.
- `info` is not like for like: nupdf's scans every page's text (up to 2,000 pages) to list the pages that need OCR, which MuPDF's script and PDF Oxide's `info` do not. `pages` is the like-for-like open and page-tree walk, which the CLI has no command for.
- Rendering: nupdf draws page contents only, MuPDF and PDF Oxide draw annotations as well. PDF Oxide's PNGs are RGBA and about twice the size of nupdf's and MuPDF's RGB ones. The CLI cannot write raw pixels, so it is in `render-png` only; in Python the raw pixmap (`render`) avoids encoding altogether.
- The CLI has no tables, annotations, signatures or page-box commands, and no speed-first text option, so those operations are measured in Python only.
- The `text` rows come from a rerun of that operation alone, after the CLI's page separators (`---` lines) were left out of its character count; the other rows are from the full runs.
- The machine was not idle: one-off spikes move single files, and medians of 3 runs and geometric means damp them but do not remove them.
