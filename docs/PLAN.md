# nupdf plan

nupdf is a rewrite of MuPDF's PDF engine in Rust. It keeps MuPDF's behavior as the reference, adds memory safety and real parallelism, and wraps everything in an interface designed for AI agents. This file is the roadmap. Work items are checkboxes; tick them in the same commit that lands the work.

Related docs: [ARCHITECTURE](ARCHITECTURE.md) · [AGENT-INTERFACE](AGENT-INTERFACE.md) · [TESTING](TESTING.md) · [DECISIONS](DECISIONS.md) · [PRIOR-ART](PRIOR-ART.md)

## Status

| Milestone | Scope | MuPDF C in scope | State |
|---|---|---|---|
| M0 | Foundation: repo, oracle, harness, bench, CI | — | done |
| M1 | Syntax, xref, repair, page tree | ~25k lines | in progress |
| M2 | Filters and encryption | ~15k | |
| M3 | Writer and structural editing | ~10k | |
| M4 | Content streams, resources, fonts | ~25k | |
| M5 | Text extraction and search | ~15k | |
| M6 | Redaction and verification | ~5k plus verifier | |
| M7 | Rendering | ~30k | |
| M8 | Agent surface: MCP, batch, eval | new | |
| M9 | Forms, annotations, OCR, tables, signatures | ~15k | |
| M10 | Hardening and public release | — | |

Line counts are rough sizes of the matching hand-written C in MuPDF 1.28.6. The data tables (CJK CMaps, glyph lists, ICC profiles) are another ~165k lines; they are converted mechanically, not rewritten.

Dependency order: M1 → M2 → M3, and M2 → M4 → M5 → M6. M7 needs M4. M8 grows from M1 onward (every command ships with its JSON contract) and is finished after M7. M9 needs M5 and M7.

## What "better than MuPDF" has to mean

These are the claims nupdf must prove with numbers before it goes public. Each maps to an exit criterion below.

1. **Parity.** On the standard corpus, nupdf produces the same objects, text and pixels as MuPDF on at least 99% of the files MuPDF opens, check by check.
2. **Safety.** No `unsafe` in nupdf's crates. No crash, hang or unbounded allocation on any input: fuzzed continuously, and run over a wild corpus of 100k+ real files.
3. **Speed.** Single-threaded throughput within 10% of MuPDF. Page-parallel operations scale to the machine's cores, which MuPDF's one-thread-per-document model cannot do without the caller restructuring work.
4. **Agents.** On a fixed task suite, agents using nupdf succeed more often, with fewer tokens and tool calls, than agents using the Python stack, pymupdf4llm-mcp or pdf_oxide.
5. **Redaction.** Zero leaks on adversarial suites, with a machine-checkable verification report, and verification never passes a leaky file.

## Principles

1. **MuPDF is the reference, and parity is measured.** Every subsystem ships with a differential check against a pinned MuPDF build. Default behavior matches MuPDF; improvements (better reading order, stricter parsing) are opt-in modes measured separately.
2. **Hostile input is the default.** No `unsafe`, no panics, explicit limits on memory, depth and time. Every new parser surface gets a fuzz target in the same milestone.
3. **Parallel by construction.** A parsed document is `Send + Sync`. Pages are processed concurrently without per-thread contexts or locks around the document.
4. **Agents are the primary user.** Versioned JSON everywhere, stable addresses for content, diagnostics as data, mutations that verify themselves. Spec: [AGENT-INTERFACE](AGENT-INTERFACE.md).
5. **Ratchet, never regress.** Parity and performance baselines are committed. CI fails when parity drops; benches flag slowdowns.
6. **Credit the source.** Modules written by following MuPDF name the files they follow and carry Artifex's copyright line. See [NOTICE](../NOTICE.md).

## Non-goals for 1.0

- Formats other than PDF. MuPDF also reads XPS, EPUB, CBZ, FB2, HTML and SVG; nupdf does not.
- Running PDF JavaScript. nupdf detects and strips it.
- XFA forms beyond detection and removal.
- A GUI viewer, or compatibility with MuPDF's C API.
- MuPDF's HTML/CSS story engine for creating PDFs.

## How a work item flows

1. Pick the next unchecked item in the current milestone.
2. Read the MuPDF files listed for it (`target/oracle/mupdf-<version>/source/...`, present after `cargo xtask oracle build`).
3. Write the tests first: unit tests, a smoke fixture in `xtask/src/fixtures.rs` when the behavior is about file syntax, and an oracle script or check when the behavior is observable through MuPDF.
4. Implement. Add the attribution header if you followed a MuPDF file.
5. Run `cargo test --workspace`, `cargo xtask harness` (smoke tier, plus the standard tier for milestone work), and the relevant bench.
6. Update the baseline only when results improved (`--update-baseline`), tick the box, commit.

## M0: Foundation (done)

- [x] Repository, AGPL-3.0-or-later license, NOTICE with MuPDF attribution, plan documents
- [x] Cargo workspace: `nupdf` (library), `nupdf-cli` (binary `nupdf`), `xtask` (tooling); `unsafe_code = "forbid"` and no-panic lints for product crates
- [x] Oracle: MuPDF 1.28.6 built from source with `cargo xtask oracle build`, version-checked on every use
- [x] Smoke corpus: 13 deterministic generated fixtures (`cargo xtask fixtures`), with their repair expectations checked against MuPDF
- [x] Differential harness: checks pair an oracle script with a `nupdf` command, run in parallel, classify outcomes, write JSON and Markdown reports, and ratchet against `harness/baselines/<tier>.json`
- [x] Bench runner: paired commands, warmup, median wall time, peak RSS via `wait4`
- [x] CLI: `nupdf info` (header version, doc id, size, diagnostics) with JSON errors and exit codes
- [x] CI: fmt, clippy, tests, fixture freshness, cargo-deny; harness workflow on the smoke tier
- [x] Corpus manifest for public suites, with sparse git fetch (`cargo xtask corpus fetch <name>`)
- [x] Prior-art survey ([PRIOR-ART](PRIOR-ART.md))

Baseline at the end of M0: `info` matches MuPDF on 11 of 12 smoke files MuPDF opens. The mismatch is `catalog-version.pdf`, because the effective version needs `/Root/Version` (M1). MuPDF cannot open `truncated.pdf` at all, and nupdf's header-only `info` still answers.

## M1: Syntax, xref, repair, page tree

MuPDF reference: `pdf-lex.c`, `pdf-parse.c`, `pdf-object.c`, `pdf-xref.c`, `pdf-repair.c`, `pdf-stream.c` (raw access), `pdf-page.c` (page tree), `pdf-crypt.c` and `fitz/crypt-*.c` (encryption, pulled forward from M2).

- [x] Fetch the standard tier (5,142 files) and record its first baseline with the existing `info` check
- [x] Lexer: whitespace and comments, all three EOL styles, MuPDF's number leniency (`pdf_lex_number`: `--5`, `1.2.3`, `.`, Google Docs' `0.0-5684342`), names with `#xx` escapes, nested literal strings with escapes, odd-length hex strings, keywords
- [x] Object parser: arrays, dictionaries (MuPDF's duplicate-key behavior and its dropped-token quirk), `n g R`, stream starts, missing `endobj`, a nesting limit
- [x] Object model: reals are `f32` and parsed by a port of MuPDF's `strtof`; dictionaries keep insertion order (DECISIONS D10, D11)
- [x] Xref: `startxref` search, classic tables including 19-byte entries and broken subsection lines, xref streams (`/W`, `/Index`), hybrid files (`/XRefStm`), object streams (`/N`, `/First`, forward-only reads), `/Prev` chains with loop detection, MuPDF's conditions for falling back to repair
- [x] Lazy, concurrent object resolution: a sharded cache of loaded objects over an immutable xref snapshot that is swapped on repair, with cycle detection
- [x] Repair as in `pdf-repair.c`: scan for objects, streams and trailers; rebuild the trailer; correct stream lengths; reclaim object-stream members; find `/Root` and `/Info`; lazy repair on a bad object; MuPDF's empty-xref state after a failed lazy repair
- [x] Effective version from `/Root/Version` (fixed the smoke mismatch)
- [x] `nupdf info` v1 (part): effective and header version, page count, xref size, revisions, repaired, encryption detected
- [x] `nupdf objects`: canonical JSON dump of every object, with raw stream lengths
- [x] Oracle scripts: `info.js` extended (page count, xref size, repair); `objects.js` added
- [x] Filters pulled forward from M2 because real xref and object streams use them: Flate (zlib-rs), PNG and TIFF predictors, ASCIIHex, ASCII85, RunLength
- [x] Revisions: `Document::revision_offsets` and `info.revision_offsets` list each xref section, newest first (redaction needs this in M6)
- [x] Bounded memory for huge declared xrefs: MuPDF allocates every declared slot in every section, so a 200-byte file declaring 8M objects costs it 357–677 MB, and a chain of ten such sections 3.2 GB. nupdf stores sections as runs and the effective table sparsely, does not cache free slots, and streams the `objects` report: 2–4 MB for all of these, with the same object counts, the same errors and MuPDF's checking order (DECISIONS D14)
- [x] Page tree: MuPDF's page map (including its `/Count` rewrite and the empty-map fallback), the slow lookup that guesses node types from `/Kids` and `/MediaBox`, cycle limits, and inherited `Resources`, `MediaBox`, `CropBox` and `Rotate` with MuPDF's Parent repair on cycles; `nupdf pages` and the `pages` check (99.8% on the standard tier, the rest encrypted or Brotli)
- [x] `Limits`: decoded stream size, xref size and object-stream size are configurable (`--max-decoded-bytes`, `--max-xref-entries`), default to MuPDF's own caps, and fail with `limit.*` codes and exit 5; structural depths stay fixed for stack safety
- [x] Diagnostics: a `warnings` check compares the distinct warnings and reported errors on MuPDF's stderr with nupdf's diagnostics; caught errors carry MuPDF's kinds (`syntax error:`, `format error:`, `library error:`); 98.9% on the standard tier
- [x] `nupdf info` v1 (rest): linearization, revisions as MuPDF counts them, and Info entries decoded with a port of MuPDF's text-string conversion; page boxes are reported per page by `nupdf pages`
- [x] `nupdf show <ref>`: one object or the trailer, with stream lengths and optional decoded data; `objects` and `pages` now carry the shared report envelope (`schema`, `doc_id`, `diagnostics`)
- [x] Fuzzing scaffold (`fuzz/`, cargo-fuzz) with targets `lex`, `parse_object`, `filters` and `open_walk`; a first 60-second run per target found nothing, and CI runs each for 30 seconds
- [x] One CPU-hour per fuzz target with no findings (the M1 exit criterion): lex 48.7M runs, parse_object 48.6M, filters 120M, open_walk 2.3M, nothing found
- [x] Fixtures: one per lexer leniency, xref repair path and page tree quirk (32 smoke fixtures, all matching MuPDF on `info`, `objects` and `pages`)
- [x] Investigate the four non-encrypted `objects` mismatches: `issue18986.pdf` and `poppler-90-0-fuzzed.pdf` came from the oracle itself, since mutool's JavaScript resolves an indirect object on any method lookup, so `objects.js` loads every object it meets as a reference; `nupdf objects` now loads them in the same order. `fuzz-16214.pdf` and the two veraPDF files come from MuPDF's endstream filter: when /Length is too short it marks itself finished as it hands over the window holding `endstream`, and whatever does not fit the reader's buffer is dropped (2305 bytes is the reader's buffer after two growths); `xref::read_all_len` models it
- [x] Standard security handler, revisions 2 to 6 (pulled forward from M2, DECISIONS D15): RC4 and AES-128/256, user and owner passwords, crypt filters (`/CF`, `/StmF`, `/StrF`, `Identity`, `/Crypt` in a filter chain) and `EncryptMetadata`. Strings decrypt as objects load and streams as they are read, with MuPDF's mutable file key (a failed attempt leaves its key in place) and its AES error handling (an error ends the data where a reader reaches it)
- [x] `--password` (user or owner; a wrong one exits 3 with `input.wrong_password`), and `encryption` (MuPDF's description string) and `needs_password` in `info`, both compared by the `info` check
- [x] Fixtures: the smoke document encrypted with each handler revision and method, with `EncryptMetadata false`, and with a user password under RC4 and AES-256 (41 smoke fixtures, all matching MuPDF)
- [x] `crypt` fuzz target: /Encrypt parsing, authentication, and string and stream decryption

**Status** (standard tier, 5,142 files, of which 19 neither tool opens):

| Check | Parity | Remaining mismatches |
|---|---|---|
| `info` (version, encryption, pages, xref size, repaired, revisions, metadata) | 100% (5,123 / 5,123) | |
| `pages` (page lookup, inherited attributes) | 100% (5,123 / 5,123) | |
| `objects` (every object, raw stream lengths) | 99.98% (5,122 / 5,123) | 1 oracle timeout (`base64image.pdf`) |
| `streams` (every non-image stream, decoded; M2) | 100% (5,123 / 5,123) | |
| `warnings` (distinct warnings and reported errors) | 99.5% (5,095 / 5,123) | Errors MuPDF reports from code paths nupdf does not run yet, mostly `invalid key in dict` and syntax errors in objects that only rendering touches |

There are no crashes, and no files that MuPDF opens but nupdf cannot. On a 300-file sample run sequentially, `nupdf info` takes 529 ms in total against MuPDF's 1,308 ms (geometric-mean speedup 2.47x, process start included; the MuPDF side also pays for its JavaScript runtime).

Checks: `info` (version, page count, boxes) and `objects` (the canonical graph, with raw stream bytes hashed).
Exit: ≥99.5% `info` parity and ≥99% `objects` parity on the standard tier. Zero nupdf crashes. One CPU-hour per fuzz target, clean. Median open-and-walk time ≤ MuPDF's. Every encryption fixture opens with the right password and fails cleanly with the wrong one (moved from M2).

Exit status: met.

## M2: Filters

The security handler moved to M1 (DECISIONS D15).

MuPDF reference: `fitz/filter-basic.c`, `filter-flate.c`, `filter-lzw.c`, `filter-predict.c`, `filter-fax.c`, `filter-dct.c`, `filter-jbig2.c`, `filter-sgi.c`, `filter-thunder.c`, `load-jpx.c`; `pdf/pdf-stream.c`.

- [x] Filter pipeline (DECISIONS D16): a port of MuPDF's stream model (`stream`), with each filter as a stage keeping its buffer size and its way of reading input, so damaged data ends where MuPDF's read ends. Whole reads keep MuPDF's compression bomb check (200 times the guessed length, or the image size, and at least 100 MB) besides `--max-decoded-bytes`; object and xref streams are read a byte at a time, as MuPDF's lexer does. This replaced M1's case-by-case models of AES and the endstream filter
- [x] ASCIIHex, ASCII85, RunLength (with MuPDF's guard against stacked decoders), LZW with EarlyChange (a port of MuPDF's decoder, its tolerances included), Flate (zlib-rs) with MuPDF's handling of corrupt data, PNG and TIFF predictors
- [x] Image decoders (DECISIONS D17): CCITTFax (a port of MuPDF's decoder), DCT (a port of libjpeg 10, the decoder MuPDF bundles, since libjpeg upsamples chroma inside its IDCT), JBIG2 with globals (hayro-jbig2, failing where jbig2dec does on what it does not implement), JPX passed through as `pdf_load_stream` does
- [x] Brotli (brotli-decompressor), which fixed the last `info` mismatch
- [x] Permissions (`pdf_has_permission`) reported by `info` under MuPDF's names, compared by the `info` check; a fixture restricts them
- [x] `nupdf streams`: every stream's decoded length and BLAKE3 hash, as MuPDF's `pdf_load_stream` reads it; `show <ref> --data` decodes one
- [x] Oracle and `streams` check: `streams.js` writes each decoded stream to a scratch directory and the harness hashes it, since mutool's JavaScript cannot. Streams with an image filter are listed on both sides but not compared; their pixels get their own check. First measurement: 98.7% (5,054 / 5,123); with the stream model, LZW and Brotli, 100% (5,123 / 5,123)
- [x] Fixtures: every non-image filter, a chain, a PNG predictor, and damaged or truncated Flate, LZW and ASCIIHex whose recovery depends on MuPDF's buffer sizes (52 smoke fixtures, all matching MuPDF on every check)
- [x] `images` check: every stream with an image filter, decoded and compared byte for byte (`images.js`, `nupdf streams --images`). First measurement, with CCITT only: 94.1%; with zune-jpeg for DCT, 95.8%; with the libjpeg port and JBIG2, 99.6% (5,105 / 5,123)
- [x] Fixtures: every image filter, with each chroma subsampling libjpeg scales in the IDCT, color transforms, restarts, complete and partial progressive scans, damaged data, Group 4 CCITT, and JBIG2 pages, globals and what jbig2dec rejects (76 smoke fixtures, all matching MuPDF on every check). xtask has its own small JPEG encoder for them
- [x] Fuzz targets per decoder: `filters` runs every non-image decoder alone and in pairs, with predictor parameters, read whole and byte by byte; `crypt` covers the RC4 and AES stages; `images` runs the DCT, CCITT and JBIG2 decoders with their parameters, seeded with image streams from the corpus. 15 minutes each, nothing found in nupdf's own code (`filters` 1.6M runs, `crypt` 5.2M, `open_walk` 0.9M, `images` 40K). `images` found endless loops and unbounded work in hayro-jbig2's symbol dictionaries, now guarded in a vendored copy (D17)

**Status** (standard tier, 5,142 files, of which 19 neither tool opens):

| Check | Parity | Remaining mismatches |
|---|---|---|
| `streams` (every non-image stream, decoded) | 100% (5,123 / 5,123) | |
| `images` (every image stream, decoded, byte for byte) | 99.6% (5,105 / 5,123) | JBIG2 test files only (D17): 14 that jbig2dec mis-decodes, 3 where it sets row padding bits, and a 1.25 GB page |

Exit: ≥99.9% decoded-stream parity for non-image filters. Image decoders within one level on 99% of pixels.

Exit status: met. Image streams match MuPDF exactly, not just within a level.

## M3: Writer and structural editing

MuPDF reference: `pdf-write.c`, `pdf-graft.c`, `pdf-clean-file.c`, `pdf-page.c` (insert and delete), `pdf-crypt.c`, `pdf-outline.c`.

- [ ] Edit overlay: copy-on-write over the parsed document; new and deleted objects; grafting between documents with a dedup map
- [x] Serializer, full rewrite (DECISIONS D18): `pdf_save_document` with default options, byte for byte. MuPDF's object printer (`print`), its dictionary sorting and two lookup paths, 16-bit generations and the free list, object streams and xref streams unpacked, streams copied raw, the security handler kept, with AES initialization vectors derived from the content instead of random
- [ ] Serializer options: garbage collection levels matching `mutool clean -g` through `-gggg`; stream compression and decompression; object streams and xref streams on write; incremental append
- [ ] Encryption on write: adding, changing and removing the security handler
- [ ] Deterministic output: `/ID` derived from content, no timestamps unless asked; atomic file replacement
- [ ] Commands: `merge`, `split`, `pages` (select, reorder, delete, rotate), `meta` (get, set, strip), `clean`, `encrypt`, `decrypt`
- [ ] Output validation: `qpdf --check`, selected Arlington PDF Model rules, and MuPDF reopening the output with identical content
- [x] Fixtures: a 120-key dictionary, every string, real and name form the printer distinguishes, a signature, null values and trailer entries, free generations past 16 bits, a null object stream member, and an empty key name MuPDF writes lossily (82 smoke fixtures)

Exit: ≥99.5% round-trip parity on the standard tier: MuPDF reads our output with the same object graph (modulo renumbering) and the same text. `merge`, `split` and `clean` are at least as fast as mutool.

## M4: Content streams, resources, fonts

MuPDF reference: `pdf-interpret.c`, `pdf-op-run.c`, `pdf-op-buffer.c`, `pdf-run.c`, `pdf-resources.c`, `pdf-xobject.c`, `pdf-pattern.c`, `pdf-shade.c`, `pdf-function.c`, `pdf-colorspace.c`, `pdf-image.c`, `pdf-layer.c`, `pdf-font.c`, `pdf-type3.c`, `pdf-cmap*.c`, `pdf-unicode.c`, `pdf-metrics.c`; `fitz/font.c`, `fitz/device.c`.

- [ ] Content lexer and parser, including inline images with MuPDF's `EI` detection
- [ ] Interpreter: graphics and text state, CTM, `q`/`Q`, `BT`/`ET`, marked content, Form XObjects with recursion limits, shadings, patterns, inline images, optional-content visibility, Type3 glyph procedures
- [ ] `Device` trait mirroring `fz_device`, plus a trace device whose output lines up with `mutool trace`
- [ ] Functions (types 0, 2, 3 and 4) and color-space definitions (conversion arrives in M7)
- [ ] Fonts: simple and composite; embedded Type1 (own parser, or hayro-font), CFF, OpenType and TrueType (skrifa/read-fonts); standard-14 metrics and MuPDF's substitute fonts; encodings, `/Differences`, the Adobe Glyph List, ToUnicode, predefined CMaps (generated from MuPDF's tables), `/W` widths, vertical writing
- [ ] Commands: `trace`, `fonts`, `images` (list and extract)
- [ ] Fuzz targets: content parser, font parsers

Exit: ≥99% page-trace parity on the standard tier: same operations, matrices within 1e-3, same glyph ids and Unicode.

## M5: Text extraction and search

MuPDF reference: `fitz/stext-device.c`, `stext-output.c`, `stext-search.c`, `stext-iterator.c`, `stext-para.c`, `stext-classify.c`, `stext-table.c`, `stext-boxer.c`, `stext-raft.c`.

- [ ] Structured text model (pages, blocks, lines, chars) with MuPDF's options: preserve ligatures, preserve whitespace, preserve spans, dehyphenate, clip to mediabox
- [ ] Output: JSON with stable ids, plain text, Markdown, JSONL per page, with output budgets
- [ ] Search: literal and regex, case- and diacritic-insensitive, across hyphenation and line breaks; hits carry quads and span ids
- [ ] Paragraph, column and table segmentation (`stext-para`, `stext-boxer`, `stext-table`), at parity first
- [ ] Page-parallel extraction with output identical at every thread count
- [ ] Opt-in improved reading order, scored against hand-labeled ground truth rather than MuPDF
- [ ] Bench pairs: against `mutool draw -F stext.json`, plus, for context, PyMuPDF, pypdf, pdfplumber, pdfminer.six, pdftotext and pdf_oxide

Exit: ≥99% of pages have the same character sequence as MuPDF's stext, and ≥98% of characters have bbox IoU ≥0.98. Single-threaded time ≤1.1× MuPDF's. ≥6× speedup on 8 cores for documents of 50+ pages.

## M6: Redaction and verification

MuPDF reference: `pdf-clean.c` (`pdf_redact_page` and helpers), `pdf-op-filter.c`, `pdf-image-rewriter.c`, `pdf-annot.c` (Redact annotations), `pdf-form.c`, `pdf-outline.c`, `pdf-nametree.c`.

- [ ] Targets: text, regex, presets (email, phone, card-like, SSN-like, IBAN), bboxes, span ids, search-hit files; resolved to quads per page
- [ ] Content filtering with MuPDF's policies: text removed; images left, removed or pixel-blanked; line art left, removed when covered, or removed when touched. Split `TJ` arrays so surrounding text keeps its position; give pages their own copies of shared Form XObjects; handle Type3 glyphs and clipping paths
- [ ] Image pixel redaction: decode, blank, re-encode with the original filter where possible
- [ ] Annotations, widgets and links inside redacted regions; optional overlay boxes
- [ ] Document scrub: Info, XMP, outline titles, named destinations, embedded files, form values, JavaScript, ActualText, Alt and structure tree, hidden optional content, thumbnails, unused objects, incremental history (always a full rewrite)
- [ ] `verify`: an independent pass over the output that checks every leak class in [AGENT-INTERFACE](AGENT-INTERFACE.md#redaction-contract)
- [ ] Adversarial fixture generator, one fixture per leak class; the `redaction` corpus tier (pdf-redaction-benchmarks)
- [ ] `--dry-run`: the redaction plan as quads now, with rendered previews after M7

Exit: zero leaks on the adversarial suite and the redaction tier, as judged by `nupdf verify`, MuPDF extraction and `pdftotext`. Remaining-text parity with MuPDF's redaction ≥99%. Tests seeded with deliberately leaky outputs prove `verify` fails them.

## M7: Rendering

MuPDF reference: `fitz/draw-*.c`, `colorspace.c`, `color-lcms.c`, `separation.c`, `pdf-shade.c`, glyph caching in `fitz/font.c`.

- [ ] Rasterizer decision: vello_cpu, tiny-skia, or a port of `draw-edge`/`draw-paint`, chosen by measured pixel parity and speed
- [ ] Paths and strokes (joins, caps, dashes), clipping, glyphs (outlines and Type3), images (every color space, decode arrays, stencil and soft masks, interpolation), shadings 1–7, patterns, transparency groups, soft masks, blend modes, knockout, overprint simulation, ICC through moxcms
- [ ] Band-parallel rendering within a page, and page-parallel across pages
- [ ] `render`: PNG output for a page or region, set DPI, and `--max-pixels` to fit vision-model budgets
- [ ] Redaction previews for `--dry-run`
- [ ] hayro as a second reference renderer, for triaging MuPDF disagreements

Exit: ≥98% of standard-tier pages within the pixel threshold of `mutool draw` at 72 and 150 dpi. Single-threaded time ≤1.2× MuPDF's, and faster than MuPDF on multiple cores.

## M8: Agent surface

- [ ] MCP server over stdio: tools mirror the commands, a document-handle cache avoids reparsing, limits and cancellation apply per call
- [ ] `batch`: globs, parallel execution, JSONL output, resumable runs
- [ ] `diff`: compare two PDFs by text, structure and pixels
- [ ] `guide` (compact usage text for models), `schema <command>` (JSON Schema), and a `SKILL.md` for Claude Code
- [ ] Freeze schemas at v1, with a changelog
- [ ] Agent eval: about 30 graded tasks (extract invoice fields, redact PII across a bundle, split by bookmarks, fill a form, find a clause, triage scanned pages). Arms: nupdf CLI, nupdf MCP, the default Python PDF skill, pymupdf4llm-mcp and pdf_oxide. Measure success, wall time, tool calls and tokens

Exit: both nupdf arms beat every baseline on success rate and on median tokens per solved task.

## M9: Coverage

- [ ] Forms: list, fill and flatten AcroForm fields; generate appearances (`pdf-form.c`, `pdf-appearance.c`); detect and strip XFA
- [ ] Annotations: list, add, delete, flatten
- [ ] OCR: rasterize, run Tesseract as a subprocess (or ocrs), add an invisible text layer; `info` routes scanned pages here
- [ ] Tables: structured rows from `stext-table` parity
- [ ] Signatures: list them, check byte-range integrity, preserve them with incremental saves
- [ ] Attachments, outlines, page labels, links; `sanitize` (JavaScript, actions, launch and URI policy)
- [ ] Optional: Python bindings (PyO3) for agents that run Python

## M10: Hardening and public release

- [ ] Wild tier: a sample of 100k+ files runs crash-free; publish the parity report
- [ ] Continuous fuzzing (apply to OSS-Fuzz), SECURITY.md, a threat model
- [ ] The public case: parity, safety, performance and agent-eval results against MuPDF and the alternatives
- [ ] Publish to crates.io, make the repository public, ship a docs site

## Cross-cutting, every milestone

- `info` learns what the new subsystem can detect (triage is the agent's first call).
- New warnings get stable diagnostic codes, documented next to the code that emits them.
- Benchmarks run for each new command; slowdowns over 5% on the local bench machine need a reason in the commit.
- Line coverage of the `nupdf` crate stays at or above 90% (`cargo llvm-cov`, starting in M1).
- Each parser surface has a fuzz target before its milestone closes.

## Risks

| Risk | Mitigation |
|---|---|
| The long tail of malformed files | MuPDF's leniency is the spec, the harness measures it file by file, and the wild tier is sampled. |
| Pixel-exact rendering parity is hard | Use thresholds rather than exact matches; triage disagreements with a second renderer (hayro). |
| Font breadth (Type1 quirks, CFF, CJK) | Reuse fontations; port MuPDF's fallbacks and substitute fonts. |
| Drift from upstream MuPDF | Oracle bumps are explicit commits. On each bump, diff the upstream files listed in ARCHITECTURE's module map and file follow-up items. |
| Performance traps (allocation-heavy object model, f64 math) | Benches from M1 on, decisions backed by profiles, open questions settled with measurements. |
| Scope creep | The non-goals above. |
| Competitors moving fast (pdf_oxide, PDFluent) | Our edge is measured parity, verified redaction and the agent eval. Publish those numbers. |
| License provenance | AGPL throughout, attribution headers on derived files, no code from projects with incompatible licenses. |

## Open questions

1. **Numbers.** Settled: f32, parsed by a port of MuPDF's `strtof` (DECISIONS D10).
2. **Dictionaries and names.** Settled for now: ordered vectors with replace-in-place (DECISIONS D11). Name interning is still open; decide from profiles.
3. **Crate split.** One `nupdf` crate until a boundary pays for itself (compile time, optional heavy dependencies). Revisit at M4 and M7.
4. **Rasterizer.** Decide at the start of M7.
5. **File access.** Read whole files into memory, or memory-map (memmap2 is `unsafe`, and the mapping breaks if the file changes)? Measure in M1. The default stays a safe read.
