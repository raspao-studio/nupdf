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
| `content` (content streams lexed and handed to a processor: the operand stack, ExtGState entries, inline images, contents arrays) | `pdf/pdf-interpret.c` (`pdf_process_stream`, `pdf_process_keyword`, `pdf_process_extgstate`), `pdf/pdf-image.c` (inline images), `pdf/pdf-stream.c` (`pdf_open_contents_stream`, `pdf_open_object_array`, `pdf_load_compressed_inline_image`), `fitz/filter-basic.c` (`fz_open_concat`), `fitz/filter-leech.c`, `fitz/image.c` (`fz_decomp_image_from_stream`) | M4 |
| `colorspace` (colorspace definitions: families, components, indexed and separation bases; ICC profiles judged by their header as lcms opens it) | `pdf/pdf-colorspace.c`, `fitz/colorspace.c` (`fz_new_indexed_colorspace`, `fz_new_icc_colorspace`), `thirdparty/lcms2/src/cmsio0.c` (`_cmsReadHeader`), `cmspcs.c` (`cmsChannelsOf`) | M4 |
| `run` (the run processor: gstates, clips, groups and soft masks, patterns, shadings, images, forms, text and Type 3 glyphs, layers, structure, metatext; a page run; the transparency scan) | `pdf/pdf-op-run.c`, `pdf/pdf-interpret.c` (`pdf_tos_*`, `pdf_process_glyph`), `fitz/font.c` (`fz_prepare_t3_glyph`, `fz_bound_t3_glyph`, `fz_render_t3_glyph_direct`), `fitz/bbox-device.c` and `fitz/list-device.c` (a Type 3 glyph's measured box), `pdf/pdf-run.c` (`pdf_run_page_contents`, `pdf_structure_type`), `pdf/pdf-page.c` (`pdf_resources_use_blending`, `pdf_load_default_colorspaces`), `pdf/pdf-pattern.c`, `pdf/pdf-xobject.c`, `pdf/pdf-struct.c` (`pdf_check_structure_tree`), `pdf/pdf-nametree.c` (`pdf_lookup_number`), `pdf/pdf-interpret.c` (soft masks in `pdf_process_extgstate`) | M4 |
| `device` (what the interpreter draws to) | `include/mupdf/fitz/device.h`, `fitz/device.c` | M4 |
| `path` (paths built and bounded as MuPDF builds them) | `fitz/path.c` | M4 |
| `image` (images as `pdf_load_image` loads them, JPEG 2000 judged by its header as openjpeg reads it) | `pdf/pdf-image.c`, `fitz/image.c` (`fz_new_image_of_size`), `fitz/load-jpx.c`, `thirdparty/openjpeg/src/lib/openjp2/jp2.c` (`opj_jp2_read_header`) | M4 |
| `function` (functions of types 0, 2, 3 and 4, loaded and evaluated) | `pdf/pdf-function.c`, `fitz/device.c` (`fz_eval_function`) | M4 |
| `shade` (shadings loaded, their functions sampled) | `pdf/pdf-shade.c` | M4 |
| `font` (font programs as FreeType opens them for MuPDF: sfnt, CFF and Type 1 faces, their charmaps, glyph names and widths; the standard 14 fonts; the CJK font's metrics) | FreeType 2: `sfnt/ttload.c`, `ttcmap.c`, `ttpost.c`, `ttmtx.c`, `sfobjs.c`, `cff/cffload.c`, `cffparse.c`, `cffobjs.c`, `type1/t1parse.c`, `t1load.c`, `t1objs.c`, `psaux/psintrp.c`, `t1decode.c`, `base/ftobjs.c`, `psnames/psmodule.c` (NOTICE); `fitz/font.c` (`ft_char_index`, `ft_name_index`), `fitz/noto.c` | M4 |
| `font::outline` (a glyph's unhinted outline control box: TrueType glyphs and composites, CFF and Type 1 charstrings) | FreeType 2: `truetype/ttgload.c`, `ttpload.c`, `psaux/psintrp.c`, `psft.c`, `pshints.c`, `psstack.c`, `cff/cffgload.c`, `base/ftoutln.c` (NOTICE) | M5 |
| `fontdesc` (PDF fonts: simple, composite and Type 3; the font program, encodings, ToUnicode, widths, the width table; glyph boxes) | `pdf/pdf-font.c`, `pdf/pdf-type3.c`, `pdf/pdf-unicode.c`, `pdf/pdf-metrics.c`, `fitz/font.c` (`fz_new_font_from_buffer`, `fz_bound_glyph`, `fz_bound_ft_glyph`, `fz_adjust_ft_glyph_width`) | M4 |
| `cmap` (CMaps: embedded, predefined, lookups) | `pdf/pdf-cmap.c`, `pdf/pdf-cmap-parse.c`, `pdf/pdf-cmap-load.c` | M4 |
| `glyphs` (glyph names to Unicode, named encodings) | `fitz/encodings.c`, `pdf/pdf-font.c` (`pdf_load_encoding`) | M4 |
| `tables` (generated: the glyph list, encodings, bidi classes) | `fitz/glyphlist.h`, `fitz/encodings.h`, `fitz/ucdn_db.h` | M4 |
| `text` (glyphs as devices get them, in spans; their bounds) | `fitz/text.c` | M4 |
| `inventory` (the fonts and images the pages' resources reach, and embedded font programs) | `tools/pdfinfo.c` (`gatherresourceinfo`, `gatherfonts`, `gatherimages`), `tools/pdfextract.c` (`savefont`) | M4 |
| `pixmap` (an image decoded to 8-bit samples; PAM and PNG writers) | `fitz/image.c` (`fz_decomp_image_from_stream`, `fz_mask_color_key`, `fz_unblend_masked_tile`, the JPEG height patch of `compressed_image_get_pixmap`), `fitz/draw-unpack.c`, `fitz/pixmap.c` (`fz_convert_indexed_pixmap_to_base`), `fitz/output-pnm.c`, `fitz/output-png.c` | M4 |
| `layer` (optional content: the default configuration's states and whether a group or membership dictionary hides content) | `pdf/pdf-layer.c` (`pdf_read_ocg`, `pdf_select_layer_config`, `pdf_is_ocg_hidden`) | M4 |
| `stext` (structured text: the stext device and its options, with the device's container stack and scissors) | `fitz/stext-device.c`, `fitz/device.c` (the container stack), `fitz/glyphbox.c`, `fitz/string.c` (`fz_chartorune`, `fz_utflen`), `fitz/util.c` (`fz_new_stext_page_from_page`); later `stext-output.c`, `stext-iterator.c`, `stext-para.c`, `stext-classify.c`, `stext-boxer.c`, `stext-raft.c`, `stext-table.c` | M5 |
| `para` (paragraph breaking: titles by underline or bold font, indents, line-height changes, trailing gaps, justified text, list items) | `fitz/stext-para.c` | M5 |
| `table` (table hunting: candidate areas from rafts of rectangles and from segments, grids from the gaps in content and the lines drawn, cells merged and scored, Table/TR/TD structures with a grid block) | `fitz/stext-table.c`, `fitz/stext-raft.c` (the flotilla) | M5 |
| `stext_tree` (the page as MuPDF's linked lists of blocks while the passes rework it, and its block iterators) | `fitz/stext-iterator.c`; the list handling of `stext-boxer.c` and `stext-table.c` | M5 |
| `order` (opt-in reading order: a recursive XY-cut of the blocks, D38) | nupdf's own | M5 |
| `segment` (page segmentation: the boxer's empty rectangles, splits at the widest gaps, `Div` structure blocks; page fills dropped) | `fitz/stext-boxer.c` | M5 |
| `extract` (content ids, spans, the text of each level, Markdown) | nupdf's own; the text follows `fitz/stext-output.c` (`do_as_text`, in `stext`) | M5 |
| `search` (search in structured text: flattening, text transforms, finders, quads) | `fitz/stext-search.c`, `fitz/util.c` (`fz_new_buffer_from_flattened_stext_page`) | M5 |
| `regexp` (MuPDF's regular expressions) | `fitz/regexp.c` | M5 |
| `unicode` (categories, combining classes, decomposition, composition, case) | `fitz/ucdn.c`, `fitz/string.c` (`fz_toupper`, `fz_tolower`, `fz_isalpha`) | M5 |
| `content_write` (content stream writer) | `pdf/pdf-op-buffer.c` (`pdf_new_output_processor`), `pdf/pdf-stream.c` (`build_compression_params`) | M6 |
| `content_filter` (sanitize filter) | `pdf/pdf-op-filter.c` (`pdf_new_sanitize_filter`) | M6 |
| `content_clean` (`clean -c` and `-s`) | `pdf/pdf-clean.c` (`pdf_filter_page_contents` and helpers), `pdf/pdf-write.c` (`clean_content_streams`) | M6 |
| `redact` | `pdf/pdf-clean.c` (`pdf_redact_page` and its filters) | M6 |
| `redact_image` | `pdf/pdf-clean.c` (`pdf_redact_image_filter_pixels`), `pdf/pdf-image.c` (`pdf_add_image`), `fitz/geometry.c` (quad tests) | M6 |
| `scrub` (document scrub after redaction) | none; nupdf's own | M6 |
| `verify` (leak classes) | none; nupdf's own | M6 |
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
- Page-level operations (extract, search, render, redaction planning) fan out over pages on scoped threads (`parallel`, D37). Each page has its own resource store and its diagnostics are captured, then reported deduplicated in a fixed order; results are collected by page index. Output is identical at every thread count, and tests enforce that.
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
