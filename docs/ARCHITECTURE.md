# Architecture

How nupdf is put together, and where each part comes from in MuPDF. Decisions that shaped this are recorded in [DECISIONS](DECISIONS.md); the roadmap is [PLAN](PLAN.md).

## Layers

```
 bytes ─▶ syntax ─────────▶ document ───────────▶ content ─────────▶ consumers ──────────▶ surface
          lexer, objects    pages, resources,     interpreter        text, search,         CLI, MCP,
          xref, repair,     filters, crypt,       emitting Device    redact, render,       batch
          revisions         fonts, edit overlay   calls              write/edit
```

Each layer only depends on the layers to its left. Consumers never see raw bytes; they see objects, decoded streams and device calls. The surface layer is thin: every command is a library function that returns a typed report, and the CLI and the MCP server only adapt it to their transport.

## Repository layout

| Path | What |
|---|---|
| `crates/nupdf` | The engine. One crate with modules until a split pays for itself (DECISIONS D4). `#![forbid(unsafe_code)]` through workspace lints. |
| `crates/nupdf-cli` | The `nupdf` binary. Argument parsing, JSON output, exit codes. |
| `xtask` | Dev tooling run with `cargo xtask`: oracle, fixtures, harness, bench, corpus. It may use `unsafe` (it calls `wait4`), and it is never shipped. |
| `oracle/` | `MUPDF_VERSION` (the pin) and `js/` scripts that run under `mutool run`. |
| `corpus/` | `smoke/` (generated and committed), `manifest.toml` (public corpora, fetched on demand). |
| `harness/baselines/` | Committed per-tier outcomes that the ratchet compares against. |
| `fuzz/` | cargo-fuzz targets (from M1). |
| `docs/` | Plan, architecture, interface contract, testing, decisions, prior art. |

## Module map

The module names are targets; they appear as the milestones land. "MuPDF" lists the files a module follows. When upstream MuPDF changes one of these files, the oracle bump review (TESTING.md) checks the matching module.

| nupdf module | MuPDF source | Milestone |
|---|---|---|
| `header` | `pdf/pdf-xref.c` (`pdf_load_version`) | M0 |
| `lex` | `pdf/pdf-lex.c` | M1 |
| `strtof` | `fitz/strtof.c`, `fitz/string.c` (`fz_atof`) | M1 |
| `parse` | `pdf/pdf-parse.c` | M1 |
| `object` | `pdf/pdf-object.c` | M1 |
| `xref` | `pdf/pdf-xref.c` (loading), `fitz/filter-basic.c` (endstream filter) | M1 |
| `repair` (scanner) | `pdf/pdf-repair.c` | M1 |
| `document` (state, loading, object streams, repair rebuild, version, page count) | `pdf/pdf-xref.c`, `pdf/pdf-repair.c`, `pdf/pdf-page.c` | M1 |
| `diag` | MuPDF's `fz_warn` calls, as data | M1 |
| `text_string` | `pdf/pdf-parse.c` (`pdf_new_utf8_from_pdf_string`), `fitz/string.c` (`fz_runetochar`), `fitz/encodings.h` (PDFDocEncoding) | M1 |
| `filter` (Flate, predictors, ASCIIHex, ASCII85, RunLength) | `pdf/pdf-stream.c` (`build_filter`), `fitz/filter-flate.c`, `fitz/filter-predict.c`, `fitz/filter-basic.c` | M1 |
| `page` | `pdf/pdf-page.c` (page map and lookup), `pdf/pdf-repair.c` (`pdf_walk_parent`, parent repair), `pdf/pdf-object.c` (`pdf_cycle`) | M1 |
| `filter` (LZW, Brotli, image decoders) | `fitz/filter-lzw.c`, `fitz/filter-brotli.c`, `fitz/filter-*.c`, `fitz/load-jpx.c`, `fitz/load-jbig2.c` | M2 |
| `crypt` | `pdf/pdf-crypt.c`, `fitz/crypt-*.c` | M2 |
| `write`, `edit`, `graft` | `pdf/pdf-write.c`, `pdf/pdf-graft.c`, `pdf/pdf-clean-file.c` | M3 |
| `content::interpret` | `pdf/pdf-interpret.c`, `pdf/pdf-op-run.c`, `pdf/pdf-run.c` | M4 |
| `device` | `fitz/device.c` | M4 |
| `resources`, `xobject`, `pattern`, `shade`, `function`, `colorspace`, `image` | `pdf/pdf-resources.c`, `pdf-xobject.c`, `pdf-pattern.c`, `pdf-shade.c`, `pdf-function.c`, `pdf-colorspace.c`, `pdf-image.c` | M4 |
| `font`, `cmap`, `encoding` | `pdf/pdf-font.c`, `pdf-type3.c`, `pdf-cmap*.c`, `pdf-unicode.c`, `pdf-metrics.c`, `fitz/font.c` | M4 |
| `layer` (optional content) | `pdf/pdf-layer.c` | M4 |
| `text` (stext) | `fitz/stext-device.c`, `stext-output.c`, `stext-iterator.c`, `stext-para.c`, `stext-classify.c`, `stext-boxer.c`, `stext-raft.c`, `stext-table.c` | M5 |
| `search` | `fitz/stext-search.c` | M5 |
| `redact`, `content::filter` | `pdf/pdf-clean.c`, `pdf/pdf-op-filter.c`, `pdf/pdf-image-rewriter.c`, `pdf/pdf-annot.c` | M6 |
| `verify` | new | M6 |
| `render` | `fitz/draw-*.c`, `fitz/colorspace.c`, `fitz/color-lcms.c`, `fitz/separation.c` | M7 |
| `forms`, `annot`, `appearance` | `pdf/pdf-form.c`, `pdf-annot.c`, `pdf-appearance.c` | M9 |
| `signature` | `pdf/pdf-signature.c` | M9 |

## Core data model

```rust
pub struct Document {
    source: Source,                 // Arc<[u8]>; the file bytes, shared, never mutated
    header: Header,                 // offset ("bias") and version
    xref: Xref,                     // one slot per object number
    trailer: Dict,
    revisions: Vec<Revision>,       // incremental-update sections, newest first
    diagnostics: Diagnostics,       // what was repaired or tolerated while opening
}

struct Slot {
    entry: XrefEntry,               // Free | InFile { offset, gen } | InStream { stream, index }
    value: OnceLock<Result<Object, ObjectError>>, // parsed on first use, from any thread
}

pub enum Object {
    Null, Bool(bool), Int(i64), Real(Real), Name(Name), String(PdfString),
    Array(Arc<[Object]>), Dict(Arc<Dict>), Ref(ObjRef), Stream(Arc<Stream>),
}

pub struct Stream { dict: Dict, data: StreamData } // StreamData::Raw(range into source) or Owned
```

Open questions in PLAN.md decide `Real` (f32 or f64), the `Dict` layout and `Name` interning. `Object` values are cheap to clone (shared `Arc`s), so handing them across threads is free.

**Editing** never mutates a `Document`. An `Edit` holds an overlay map from `ObjRef` to new objects, plus new object numbers, over an immutable base. The writer serializes the overlay either as a full rewrite or as an incremental update. Readers on other threads keep working against the base while an edit is in progress.

## Concurrency

MuPDF needs a cloned `fz_context` per thread and serializes access to a document. Its documented pattern for parallelism is to build display lists on one thread and rasterize them on others. nupdf instead makes the document itself shareable:

- `Document: Send + Sync`. Object resolution is lazy and lock-free (`OnceLock` per slot). Two threads that race to parse the same object do the same work, and one result wins.
- Shared caches (fonts, decoded images, glyphs) are concurrent maps with size bounds and are keyed by `ObjRef`.
- Page-level operations (extract, search, render, redaction planning) fan out over pages with rayon. Results are collected by page index, so output is identical at every thread count, and a test enforces that.
- `batch` adds file-level parallelism on top. The MCP server keeps open documents in a handle cache, so repeated calls do not reparse.
- Rendering adds band-level parallelism within a page in M7.

## Safety and limits

- `unsafe_code = "forbid"` in every product crate. Dependencies may contain audited `unsafe` (zlib-rs, blake3), but nupdf's own code has none.
- No panics on input. `unwrap`, `expect` and `panic!` are lint warnings (errors in CI) outside tests. Fuzzing catches the rest: out-of-bounds slicing, arithmetic overflow and stack overflow.
- No unbounded recursion. Nested arrays and dictionaries, Form XObjects, reference chains and page trees all carry explicit depth limits, because a stack overflow aborts the process.
- A `Limits` value travels with every document (`Document::open_with`): today the decoded size of a stream, the xref size and the size of an object stream, defaulting to MuPDF's own caps. Pixel counts, time and a cancellation token join it as rendering and long operations arrive. Structural depths (object nesting 512, page tree and reference chains 256) are fixed constants, because their purpose is to keep the stack safe.
- PDF JavaScript is never executed.

## Errors and diagnostics

Two channels, kept apart deliberately:

- `Result<T, Error>` only for failures that stop the operation (unreadable file, wrong password, limit exceeded). Errors have stable codes such as `input.not_pdf` and `limit.decompressed_size`.
- `Diagnostics` for everything that was tolerated, repaired or skipped: `xref.repaired`, `font.missing_tounicode`, `stream.length_mismatch`. Each carries a severity and a location (page, object, byte offset). They are MuPDF's `fz_warn` turned into data. Reports include them, so an agent knows when a result is degraded and why.

## Dependency policy

Pure Rust, widely used, permissive licenses compatible with AGPL (cargo-deny enforces the allow-list). No C libraries in product crates; Tesseract runs as a separate process. Planned dependencies, from the prior-art survey:

| Need | Crate |
|---|---|
| Deflate | zlib-rs |
| LZW | weezl |
| JPEG | zune-jpeg |
| JPEG 2000, JBIG2, CCITT | hayro-jpeg2000, hayro-jbig2, fax / hayro-ccitt |
| Fonts | skrifa / read-fonts (fontations), subsetter; Type1 from hayro-font or our own parser |
| Color management | moxcms |
| Rasterizer | vello_cpu or tiny-skia (decided in M7) |
| Crypto | RustCrypto: aes, cbc, rc4, md-5, sha2 |
| Parallelism | rayon |
| Hashing (doc ids) | blake3 |
| OCR (optional) | ocrs, or Tesseract as a subprocess |

hayro's own crypto is marked unaudited by its author, and nupdf also needs to encrypt, so it uses RustCrypto instead.

## Determinism

Same input, arguments and nupdf version give byte-identical JSON and PDF output. That means no timestamps unless asked for, `/ID` derived from content, object numbering and dictionary key order that are stable, and parallel results ordered by page. The harness and the agent eval both rely on this.
