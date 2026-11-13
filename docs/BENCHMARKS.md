# Benchmarks

Speed of nupdf against MuPDF, the reference, and pdf_oxide. How the runs work is in [TESTING](TESTING.md#benchmarks); the milestone measures against MuPDF alone (render, text, scaling, `clean`, `merge`, `split`) are in [PLAN](PLAN.md) beside each milestone's exit. The raw data for this page is in `bench/results/2026-11-13-pdf_oxide/`.

## Against MuPDF and pdf_oxide, 2026-11-13

```bash
cargo xtask bench --tier standard --sample 200 --runs 3 --against pdf_oxide
cargo xtask bench --tier long --runs 3 --against pdf_oxide
```

- **Machine**: Apple M5 Max (18 cores: 6 "Super" and 12 "Performance", as `sysctl hw.perflevel*` names them), 128 GB, macOS 27.2.1, on mains power, with other work on the machine (load average 6 to 10 on 18 cores). Every process runs on one thread, one at a time.
- **Versions**: nupdf at `b028ca9` (release build), MuPDF 1.28.6 (the pinned oracle), pdf_oxide 0.3.78 from PyPI on CPython 3.12.11 (`oracle/bench/requirements.txt`).
- **Files**: `standard` is an evenly spaced 200 of the standard tier's files, mostly small (nupdf's median process time is 2 to 4 ms, mostly its start); `long` is the 30 documents of 96 to 362 pages merged from them (3,414 pages).
- **Medians** of 3 timed runs after a first run that must succeed.

What each side runs and writes:

| Operation | nupdf | MuPDF | pdf_oxide |
|---|---|---|---|
| info | `info`: JSON with version, page count, objects, permissions, metadata, and every page's text scanned for pages that need OCR | `info.js`: the same without the scan | `PdfDocument`, `version()`, `page_count` |
| pages | `pages`: each page's object and inherited MediaBox, CropBox, Rotate, Resources | `pages.js`: the same | `page_media_box`, `page_crop_box`, `page_rotation` of every page, as JSON |
| text | `extract --format text` to stdout | `draw -F txt` to a file | `extract_text(i)` of every page, to a file; `text-fast` with `extract_tables=False` |
| render | `render --format ppm`, 72 dpi, without annotations | `draw -N -r 72`, PPM, with annotations | `render_pixmap(i, dpi=72)`, written as PAM (RGBA), with annotations |
| tables | `tables`: JSON rows of cells | `tables.js` (table hunting) | `extract_tables(i)` as JSON |
| annots | `annots` (widgets left out) | `annots.js` | `get_annotations(i)` (widgets included) as JSON |
| forms | `forms`: each page's widgets | `forms.js` | `get_form_fields()` as JSON |
| signatures | `signatures`: byte ranges, digest | `signatures.js` | `signatures()`, each `verify_detached` |

Ratios are geometric means of each tool's time over nupdf's: above 1x, nupdf is faster. "Process" is wall time of the whole process, start included, which is what an agent pays per call. "In process" leaves the start out on both sides: pdf_oxide's own clock from opening the document to the last byte written (no interpreter, no `import`), and nupdf's process time less the fastest of 21 runs of `nupdf --version` (1.7 ms in the standard run, 2.4 ms in the long one), taken as at least 0.5 ms, both of which err against nupdf. The totals column is the ratio of summed in-process times, which large files weigh more.

A file counts in a ratio when both tools it compares did it: exit 0 and, for pdf_oxide, no page raised. For info, pages, text and render, nupdf and pdf_oxide must also agree (the same page count; both with text or both without; the same number of pages with ink), so a fast empty answer is not a win. `mutool` exits 1 when any page has an error, even when it writes every page, which counts as a failure, as does any run over 300 s.

### Standard tier, 200 files

| Operation | MuPDF / nupdf, process | pdf_oxide / nupdf, process | pdf_oxide / nupdf, in process | in-process totals | Failed: MuPDF, nupdf, pdf_oxide | Disagree |
|---|---|---|---|---|---|---|
| info | 1.60x | 12.5x | 0.18x | 0.12x | 1, 1, 4 | 2 |
| pages | 1.99x | 15.3x | 0.43x | 0.43x | 1, 1, 5 | 1 |
| text | 1.72x | 13.0x | 0.58x | 0.85x | 10, 1, 4 | 6 |
| text-fast | 1.75x | 13.4x | 0.69x | 0.90x | 10, 1, 4 | 6 |
| render | 1.31x | 11.1x | 4.36x | 3.81x | 10, 1, 7 | 15 |
| tables | 2.07x | 12.9x | 0.49x | 0.79x | 1, 1, 4 | |
| annots | 1.99x | 15.5x | 0.32x | 0.30x | 1, 1, 5 | |
| forms | 2.06x | 15.9x | 0.54x | 0.52x | 1, 1, 4 | |
| signatures | 2.03x | 16.0x | 0.62x | 0.61x | 1, 1, 4 | |

pdf_oxide's interpreter start and import take 34 to 38 ms (median per operation), which is most of its process time on these files: its work on a small file is a fraction of a millisecond. On the same small files nupdf's process is 2 to 4 ms, and what is left after its start includes hashing the whole file for `doc_id` and writing a versioned report, so in process pdf_oxide is ahead on everything but rendering. The standard sample has few large files (nupdf over 20 ms: 9 for render, 1 or 2 for the others), and on those nupdf is ahead in process (render 2.8x); the long tier measures that properly.

### Long tier, 30 documents of 96 to 362 pages

| Operation | MuPDF / nupdf, process | pdf_oxide / nupdf, process | pdf_oxide / nupdf, in process | in-process totals | Failed: MuPDF, nupdf, pdf_oxide | Disagree |
|---|---|---|---|---|---|---|
| info | 0.16x | 1.31x | 0.03x | 0.01x | 0, 0, 0 | 0 |
| pages | 1.54x | 13.1x | 10.6x | 8.11x | 0, 0, 0 | 0 |
| text | 1.43x | 3.67x | 1.61x | 1.54x | 10, 0, 0 | 0 |
| text-fast | 1.48x | 3.43x | 1.45x | 1.33x | 10, 0, 0 | 0 |
| render | 1.46x | 2.94x | 1.37x | 0.26x | 10, 0, 5 | 10 |
| tables | 2.93x | 2.72x | 1.23x | 0.33x | 0, 0, 0 | |
| annots | 1.42x | 9.81x | 3.02x | 2.59x | 0, 0, 0 | |
| forms | 1.44x | 8.53x | 0.46x | 0.26x | 0, 0, 0 | |
| signatures | 1.40x | 8.37x | 0.58x | 0.35x | 0, 0, 0 | |

On these documents the work outweighs starting a process, and in geometric mean nupdf is ahead of pdf_oxide in process on text (1.6x, 1.45x against its speed-first mode), rendering (1.4x), tables (1.2x), page-tree walks (10.6x) and annotations (3.0x). Totals are swayed by a few documents where nupdf is far slower than its median:

- `long-018.pdf`: nupdf renders it in 55.7 s against pdf_oxide's 1.8 s (MuPDF was still drawing it after 10 minutes, and the bench's 300 s limit counts it as failed), and `long-002.pdf` in 2.4 s against 0.57 s. These two make render's total 0.26x.
- `long-012.pdf`: nupdf's `tables` takes 12.2 s, MuPDF's table hunting 5.9 s, pdf_oxide 0.27 s (finding 1 table where nupdf and MuPDF find 16).
- `long-006.pdf`: one page takes nupdf 2.3 to 2.9 s and MuPDF 2.0 s for text, tables and `info`; pdf_oxide reads the whole document's text in 45 ms, with about as many characters (7,397 against 6,992).
- `info` is 0.16x of MuPDF's speed here because it scans every page's text (above), 2.3 s on `long-006.pdf`.
- `forms` and `signatures` walk every page's widgets on nupdf's side and read the AcroForm on pdf_oxide's; these documents have no fields, so pdf_oxide's work is close to nothing.

pdf_oxide fails to render 5 of the 30 (a page raises), and disagrees on which pages have ink on 10 more, by 1 to 8 pages each, so render's in-process ratio is over 15 documents.

### What the outputs show

- **Failures.** nupdf fails one standard file (`crypt_filters.pdf`, which MuPDF fails too). pdf_oxide raises on 4 to 7 depending on the operation: always on `repeat_viewer_ref.pdf`, `bug1020226.pdf` and `issue-119.pdf` (damaged files MuPDF and nupdf read) and `enc-XI-R6,V5,U=wwwww,O=wwwww.pdf` (AES-256 with a user password; nupdf and MuPDF report that it needs one), and on some operations `bug_552046.pdf`, `bad-data-out.pdf` and `crypt_filters.pdf`.
- **Disagreements** are mostly what each tool counts. Text: on 4 files pdf_oxide has text where nupdf has none, each a file with annotations, whose appearance text nupdf's `extract` leaves out (`mutool draw -F txt` draws annotations, so MuPDF has it too); on 2 (`bad24.pdf`, a `.notdef` test) nupdf has text and pdf_oxide none. Render: on 12 of 15 pdf_oxide has ink where nupdf has none, almost all on pages with annotations, which pdf_oxide and MuPDF draw and nupdf's `render` does not; on 3 nupdf has ink where pdf_oxide's page is blank. Info and pages: pdf_oxide counts 1 or 2 pages in two files whose page tree has none (MuPDF and nupdf count 0).
- **Counts** that are not checked differ by design: pdf_oxide's annotations include widgets and links (93 against nupdf's 40 on the standard tier; 33,187 links against 0 on the long tier), its tables are found its own way (9 against 40), and its form fields are the AcroForm's terminal fields rather than each page's widgets.
- **Correctness seen in passing.** pdf_oxide's `page_media_box` does not inherit: on `corpus/smoke/stext.pdf`, whose pages inherit a 300 by 300 MediaBox from the page tree, it returns US Letter. On `long-005.pdf` its text has 61% of nupdf's characters, which the both-have-text check does not catch.

### Caveats

- The standard tier is small files, so process times there mostly measure starting a process, and the in-process figures for nupdf are a subtraction of two numbers of a few milliseconds. The long tier and the totals are the better measure of the engines.
- `info` is not like for like: nupdf's scans every page's text (up to 2,000 pages) to list the pages that need OCR, which MuPDF's script and pdf_oxide's `page_count` do not. `pages` is the like-for-like open and page-tree walk.
- `render`: nupdf draws page contents only, MuPDF and pdf_oxide draw annotations as well, and pdf_oxide's pixmap is RGBA rather than RGB. pdf_oxide's PNG and JPEG output would add encoding, so the raw pixmap is the fairest path it has.
- pdf_oxide is measured through its Python bindings, the interface the agent eval used. Its in-process time is native Rust plus a thin binding layer; a call per page crosses that layer once.
- The machine was not idle: one-off spikes move single files, and medians of 3 runs and geometric means damp them but do not remove them.
