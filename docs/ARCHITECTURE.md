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
| `xtask` | Dev tooling run with `cargo xtask`: oracle, fixtures, harness, bench, corpus, and the Arlington rules the validation checks apply, from the definitions vendored in `xtask/data/arlington/`. It may use `unsafe` (it calls `wait4`), and it is never shipped. |
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
| `slots` (sparse xref storage) | new; MuPDF allocates every declared slot | M1 |
| `crypt` (standard security handler) | `pdf/pdf-crypt.c`, `fitz/crypt-arc4.c`, `fitz/filter-basic.c` (RC4 and AES filters) | M1 |
| `repair` (scanner) | `pdf/pdf-repair.c` | M1 |
| `document` (state, loading, object streams, repair rebuild, version, page count; the edit overlay, new documents) | `pdf/pdf-xref.c` (also `pdf_update_object`, `pdf_xref_ensure_incremental_object`, `pdf_create_object`, `pdf_delete_object`, `pdf_update_stream`, `pdf_create_document`), `pdf/pdf-repair.c`, `pdf/pdf-page.c` | M1, M3 |
| `diag` | MuPDF's `fz_warn` calls, as data | M1 |
| `text_string` | `pdf/pdf-parse.c` (`pdf_new_utf8_from_pdf_string`), `fitz/string.c` (`fz_runetochar`), `fitz/encodings.h` (PDFDocEncoding) | M1 |
| `stream` (MuPDF's stream model, the endstream filter) | `include/mupdf/fitz/stream.h`, `fitz/stream-read.c`, `fitz/stream-open.c`, `fitz/filter-basic.c` | M2 |
| `filter` (Flate, LZW, Brotli, RunLength, ASCIIHex, ASCII85, predictors, RC4 and AES; the filter chain) | `pdf/pdf-stream.c` (`build_filter`, `build_compression_params`), `pdf/pdf-colorspace.c` (`pdf_guess_colorspace_components`), `fitz/compressed-buffer.c`, `fitz/filter-basic.c`, `fitz/filter-flate.c`, `fitz/filter-lzw.c`, `fitz/filter-brotli.c`, `fitz/filter-predict.c` | M1, M2 |
| `page` | `pdf/pdf-page.c` (page map and lookup), `pdf/pdf-repair.c` (`pdf_walk_parent`, parent repair), `pdf/pdf-object.c` (`pdf_cycle`) | M1 |
| `fax` (CCITTFax) | `fitz/filter-fax.c` | M2 |
| `dct` (DCTDecode around the JPEG decoder) | `fitz/filter-dct.c` | M2 |
| `jpeg` (JPEG decoder) | libjpeg 10 as bundled with MuPDF: `jdapimin.c`, `jdapistd.c`, `jdmaster.c`, `jdinput.c`, `jdmarker.c`, `jdhuff.c`, `jdarith.c`, `jaricom.c`, `jdcoefct.c`, `jddctmgr.c`, `jidctint.c`, `jdmainct.c`, `jdsample.c`, `jdcolor.c`, `jutils.c` (NOTICE) | M2 |
| `jbig2` (JBIG2Decode around hayro-jbig2) | `fitz/filter-jbig2.c`, `pdf/pdf-stream.c` (`pdf_load_jbig2_globals`); jbig2dec's refusals from `jbig2.c` and `jbig2_segment.c` | M2 |
| `names` (the names MuPDF interns, which take their own dictionary lookup path) | `include/mupdf/pdf/name-table.h` | M3 |
| `print` (object syntax as MuPDF writes it) | `pdf/pdf-object.c` (`pdf_print_obj`), `fitz/printf.c` (`fmtfloat`), `fitz/ftoa.c` (`fz_grisu`) | M3 |
| `write` (full and incremental saves, and the read-back check) | `pdf/pdf-write.c` | M3 |
| `rearrange` (keeping some pages in a given order) | `pdf/pdf-clean-file.c` (`pdf_rearrange_pages`), `pdf/pdf-nametree.c` (`pdf_load_name_tree`), `pdf/pdf-page.c` (`pdf_flatten_inheritable_page_items`), `pdf/pdf-repair.c` (`pdf_parent_root`) | M3 |
| `edit` (document information, removing metadata, page rotation) | `pdf/pdf-xref.c` (`pdf_set_metadata`), `pdf/pdf-parse.c` (`pdf_new_text_string`) | M3 |
| `garbage` (garbage collection on save: marking, merging duplicates, renumbering) | `pdf/pdf-write.c`, `pdf/pdf-object.c` (`pdf_objcmp`, `pdf_hash_obj`) | M3 |
| `deflate` (zlib's `compress2`, for the streams `clean -z` writes) | zlib 1.3.2 as bundled with MuPDF: `compress.c`, `deflate.c`, `trees.c`, `adler32.c` (NOTICE) | M3 |
| `fax_encode` (CCITT G4 encoder, for 1-bit images `clean -z` writes) | `fitz/encode-fax.c`, `fitz/buffer.c` (`fz_append_bits`) | M3 |
| `graft` (copying objects and pages between documents) | `pdf/pdf-graft.c` | M3 |
| `insert` (putting a page into the page tree, page labels shifted) | `pdf/pdf-page.c` (`pdf_insert_page`, `pdf_adjust_page_labels`), `pdf/pdf-repair.c` (`pdf_walk_parent`) | M3 |
| `geometry` (points, rectangles, matrices in single precision) | `fitz/geometry.c` | M3 |
| `link` (links read as URIs and written back: destinations, actions, file specifications, `pdf_create_link`) | `pdf/pdf-link.c`, `pdf/pdf-annot.c` (`pdf_create_link`), `pdf/pdf-nametree.c` (`pdf_lookup_dest`), `pdf/pdf-page.c` (`pdf_lookup_page_number`), `fitz/link.c`, `fitz/string.c` (URI encoding, `fz_cleanname`) | M3 |
| `outline` (MuPDF's outline iterator, repair pass included) | `pdf/pdf-outline.c` | M3 |
| `merge` (`mutool merge`: pages, external links, outline items) | `tools/pdfmerge.c` | M3 |
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
    state: RwLock<Arc<State>>,      // xref entries and the object cache; replaced whole on repair
    trailer: Dict,
    revisions: Vec<Revision>,       // incremental-update sections, newest first
    diagnostics: Diagnostics,       // what was repaired or tolerated while opening
}

struct State {
    entries: Slots,                 // 16-byte entries; an array, or uniform ranges plus a map for huge tables
    cache: Cache,                   // loaded objects, in 64 shards behind RwLocks
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

- `Document: Send + Sync`. Object resolution is lazy. Loaded objects go into a cache split into 64 shards, each behind an `RwLock`, so readers rarely contend. Two threads that race to load the same object may both parse it, and the first result stored wins. A repair builds a new `State` and swaps it in; readers holding the old one finish against it.
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
| Deflate | zlib-rs to decode; own port of zlib's deflate to encode (DECISIONS D20) |
| LZW | own port of MuPDF's decoder (DECISIONS D16) |
| Brotli | brotli-decompressor |
| JPEG | own port of libjpeg (DECISIONS D17) |
| JBIG2 | hayro-jbig2 |
| CCITT | own port of MuPDF's decoder |
| JPEG 2000 | hayro-jpeg2000 (M4) |
| Fonts | skrifa / read-fonts (fontations), subsetter; Type1 from hayro-font or our own parser |
| Color management | moxcms |
| Rasterizer | vello_cpu or tiny-skia (decided in M7) |
| Crypto | RustCrypto: aes, md-5, sha2 (CBC and RC4 are small enough to write, and MuPDF's padding and error rules need them open-coded) |
| Parallelism | rayon |
| Hashing (doc ids) | blake3 |
| OCR (optional) | ocrs, or Tesseract as a subprocess |

hayro's own crypto is marked unaudited by its author, and nupdf also needs to encrypt, so it uses RustCrypto instead.

## Determinism

Same input, arguments and nupdf version give byte-identical JSON and PDF output. That means no timestamps unless asked for, `/ID` derived from content, object numbering and dictionary key order that are stable, and parallel results ordered by page. The harness and the agent eval both rely on this.
