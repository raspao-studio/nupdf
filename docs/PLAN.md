# nupdf plan

nupdf is a rewrite of MuPDF's PDF engine in Rust. It keeps MuPDF's behavior as the reference, adds memory safety and real parallelism, and wraps everything in an interface designed for AI agents. This file is the roadmap. Work items are checkboxes; tick them in the same commit that lands the work.

Related docs: [ARCHITECTURE](ARCHITECTURE.md) · [AGENT-INTERFACE](AGENT-INTERFACE.md) · [TESTING](TESTING.md) · [DECISIONS](DECISIONS.md) · [PRIOR-ART](PRIOR-ART.md)

## Status

| Milestone | Scope | MuPDF C in scope | State |
|---|---|---|---|
| M0 | Foundation: repo, oracle, harness, bench, CI | — | done |
| M1 | Syntax, xref, repair, page tree | ~25k lines | done |
| M2 | Filters and encryption | ~15k | done |
| M3 | Writer and structural editing | ~10k | done |
| M4 | Content streams, resources, fonts | ~25k | done |
| M5 | Text extraction and search | ~15k | |
| M6 | Redaction and verification | ~5k plus verifier | done |
| M7 | Rendering | ~30k | done |
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
| `warnings` (distinct warnings and reported errors) | 99.5% (5,096 / 5,123) | Errors MuPDF reports from code paths nupdf does not run yet, mostly `invalid key in dict` and syntax errors in objects that only rendering touches |

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

- [x] Edit overlay (DECISIONS D23): copy-on-write over the parsed document, seen by every lookup; replaced, changed and new objects, with the generations MuPDF's incremental section gives them; new streams and trailer
- [x] Grafting between documents with a dedup map, and deleting objects (DECISIONS D25): `pdf_graft_mapped_object` and `pdf_graft_mapped_page` (`graft`) into a document from `pdf_create_document`, `pdf_insert_page` with its page label adjustment (`insert`), and `pdf_delete_object` in the overlay. A null object is no object, as in MuPDF, so a reference to one that is null, free or cannot be loaded leaves a free entry
- [x] Serializer, full rewrite (DECISIONS D18): `pdf_save_document` with default options, byte for byte. MuPDF's object printer (`print`), its dictionary sorting and two lookup paths, 16-bit generations and the free list, object streams and xref streams unpacked, streams copied raw, the security handler kept, with AES initialization vectors derived from the content instead of random
- [x] Garbage collection (DECISIONS D19): `clean -g` through `-gggg` as MuPDF's `do_garbage` (`garbage`). Marking from the trailer, with references to nothing made null and indirect stream lengths baked; merging duplicates by its CRC-32 of the in-memory object and `pdf_objcmp`, until nothing changes; renumbering by key, quirks included; objects encrypted again under their new numbers
- [x] Stream options (DECISIONS D20): `clean -d`, `-z`, `-f`, `-i`, `-a` and `-e` as MuPDF's `copystream` and `expandstream`. Deflate by a port of zlib 1.3.2 (`deflate`), 1-bit images by a port of MuPDF's CCITT G4 encoder (`fax_encode`), ASCIIHexDecode taken off before compressing and put on binary streams for `-a`, filter arrays edited in place where streams share them, and MuPDF's pretty-printing default. Brotli (`-zz`) refused
- [x] Object streams and xref streams on write (DECISIONS D21): `clean -Z` as `gather_to_objstms` and `writexrefstream`, with the version raised to 1.5. What is packed and where the save fails follow MuPDF's loop: unused objects packed at `-g`, entries loaded before they are looked at, and the xref stream numbered after every object
- [x] Incremental append (DECISIONS D27): `--incremental` on the edits, as MuPDF's incremental save: the file as it was, the objects its edits touch, an xref in the style of the last one read, /Prev back; refused where MuPDF refuses
- [x] Encryption on write (DECISIONS D22): `clean -D`, and `-E` with `-O`, `-U` and `-P`, as `pdf_new_encrypt` and `create_encryption_dictionary`, for RC4 40 and 128, AES-128 and AES-256
- [x] Atomic file replacement: written beside the target, then renamed
- [x] Deterministic output: nothing nupdf writes is random. The `/ID` a new security handler needs, the AES-256 file key, salts and `/Perms`, and AES initialization vectors (D18) are derived from the input; no timestamps are written
- [x] `clean` with MuPDF's defaults: `-o` or `--dry-run`, and a read-back check that refuses output that does not open, needs a repair, or reads back differently (exit 4, `--no-verify` to skip)
- [x] Commands (DECISIONS D24): `split`, `pages` (select, reorder, delete, rotate), `meta` (get, set, strip), `encrypt`, `decrypt`, and `clean`'s options. Each writes as `clean` does, and a document the empty password does not open is refused without `--password`, as `pdf_clean_file` refuses it
- [x] Commands: `merge`'s links and outline items, copied as `mutool merge` copies them (DECISIONS D26): links read as MuPDF's URIs through each page's transform and the external ones recreated (`link`, `geometry`), the outline walked and extended with MuPDF's iterator (`outline`), and pdfmerge.c's copying (`merge`)
- [x] `meta-set`, `meta-strip`, `pages-rotate` and `pages-select-rotate` checks: the edit made through MuPDF's script interface, then saved as `mutool clean` saves
- [x] `meta-set-incremental`, `meta-strip-incremental` and `pages-rotate-incremental` checks: the edit appended, against MuPDF's incremental save of it
- [x] `merge`, `merge-twice` and `merge-reverse` checks: each file merged whole, twice, and in reverse, against `mutool merge` mirrored through MuPDF's script interface: pages grafted, external links recreated, outline items copied
- [x] `clean` check: BLAKE3 of the output against MuPDF's in reproducible mode, the comment line naming the writer swapped first
- [x] `roundtrip` check: each side's `clean` output read back by MuPDF (`objects.js` with a scratch directory), comparing the object graph, raw stream data, repairs and warnings
- [x] `clean-g` through `clean-gggg` checks, `clean` at each garbage collection level, and `roundtrip-gggg`
- [x] `clean-d`, `clean-z`, `clean-ze`, `clean-a`, `clean-dfi` and `clean-dzgg` checks: `clean` with the stream options, alone and combined with renumbering
- [x] `clean-Z`, `clean-Zg` and `clean-Zggz` checks: object streams alone, with unreachable objects packed, and with renumbering and compression
- [x] `clean-D` check, and `roundtrip-E40`, `roundtrip-E128`, `roundtrip-Eaes128` and `roundtrip-Eaes256`: MuPDF reads each side's newly encrypted output back
- [x] `clean --pages` (D23): `mutool clean`'s page arguments, as `pdf_rearrange_pages`, with the `clean-pages-reverse`, `clean-pages-first` and `clean-pages-rest` checks
- [x] Output validation (DECISIONS D28): `qpdf --check` and selected Arlington PDF Model rules on what `clean`, `clean -Z`, `merge` and an incremental edit write, each side's output judged, and nupdf's matching when the validator says the same of it as of MuPDF's
- [x] Fixtures: a 120-key dictionary, every string, real and name form the printer distinguishes, a signature, null values and trailer entries, free generations past 16 bits, a null object stream member, and an empty key name MuPDF writes lossily; for garbage collection, unreachable objects and references to nothing, duplicates that merge only once others have, the renumbering quirks, and an RC4 document renumbered; for the stream options, unfiltered 1-bit images, a mask, an 8-bit image, a font file and XML metadata, ASCIIHexDecode in each form MuPDF takes off, filter arrays shared by reference, an empty `/DecodeParms` array, and a `/Filter` MuPDF's lookup misses; for object streams, what gathering leaves out, two streams' worth of objects, and xref gaps that do and do not fail the save; an empty stream, which AES encryption loses; for page selection, a nested page tree with outlines, named destinations, links and a field across pages; for the editing commands, pages that are direct dictionaries and /Info in an xref stream's trailer; for merge, an indirect /Length, a font pages share, references to a null object, a free number and one past the xref, and links and outline items of every kind it copies (99 smoke fixtures)
- [x] Fuzz: `open_walk` also writes the document, at a garbage collection level, with or without object streams, with the stream options and the security handler the input picks, checks that writing is deterministic, and reads the output back; then keeps some pages, as `clean --pages` does, makes the edits `meta` and `pages --rotate` make, and writes that, appended to the file or not; and merges its first pages into a new document, as `merge` does, links and outline included, and writes that. 15 minutes at the default level (22K runs), then 15 across all levels (7K runs), then 15 with the stream options too (7.6K runs), then 15 with object streams too (7.8K runs), then 15 with a new or removed security handler too (4.2K runs), then 15 that also keep some pages first (4.8K runs), then 15 with the edits too (4.4K runs), then 15 merging too (4.6K runs), then 15 with links and outlines (6K runs), then 15 appending the edits instead (an out-of-memory: repair took an entry per object number, 1.2 GB for a 300-byte file naming object 8,149,045; fixed, after which 15 more ran 225K runs); `encode` deflates and fax-encodes any input and decodes it back, 10 minutes (659K runs). Nothing found

**Status** (standard tier, 5,142 files, of which 101 neither tool writes: 56 that neither can, and 45 that the empty password does not open, which `mutool clean` refuses):

| Check | Parity | Remaining mismatches |
|---|---|---|
| `clean` (output bytes) | 99.6% (5,020 / 5,040) | AES files only (D18): random initialization vectors in MuPDF's output. Besides those, MuPDF fails to save `issue20453.pdf` after its repair; nupdf writes it |
| `roundtrip` (MuPDF reads each output back) | 99.96% (5,037 / 5,039) | Random IVs again, on 2 files where MuPDF's own output does not read back exactly: an empty stream under AES, which the output loses (D22), and streams under an Identity crypt filter. One more file times out on both sides |
| `clean-g` to `clean-gggg` (output bytes at each level) | 99.6% (5,058 / 5,078) each | The same 20 AES files as `clean`. Collecting lets both tools write 37 files they cannot write without it, whose unloadable objects nothing reaches, and `issue20453.pdf` |
| `roundtrip-gggg` | 99.96% (5,042 / 5,044) | The 2 files `roundtrip` differs on. For 33 more files MuPDF cannot open either output: their trailer has no /Root, so nothing is reachable and the xref is empty |
| `clean-d` (decompress) | 99.3% (4,999 / 5,036) | The 20 AES files (random IVs), and 17 whose image decoding differs in the `images` check, mostly JBIG2 refinement and halftone regions. MuPDF fails to save `issue20453.pdf`, as without `-d`, and `bomb_giant.pdf`, on a compression bomb that nupdf's JBIG2 decoder refuses up front (D20) |
| `clean-z`, `clean-ze` (compress, default effort and 100) | 99.6% (5,019 / 5,039) each | The 20 AES files |
| `clean-a` (hex) | 99.6% (5,020 / 5,040) | The 20 AES files |
| `clean-dfi` (decompress all but fonts and images) | 99.6% (5,017 / 5,037) | The 20 AES files |
| `clean-dzgg` (decompress and compress after renumbering) | 99.3% (5,040 / 5,074) | The 20 AES files, and 14 of the image decoding differences; `bomb_giant.pdf` as with `-d` |
| `clean-Z`, `clean-Zg` (object streams, alone and at `-g`) | 99.6% (4,998 / 5,018) each | The 20 AES files. MuPDF fails to save `issue20453.pdf`, as without `-Z`; with `-g` it fails where nupdf writes, since its repair during the save differs. Both fail on 22 files whose xref skips an object number, and at `-g` on 37 more with unloadable objects nothing reaches (D21) |
| `clean-Zggz` (object streams after renumbering, compressed) | 99.6% (5,057 / 5,077) | The 20 AES files |
| `clean-D` (without encryption) | 100% (5,040 / 5,040) | |
| `roundtrip-E40`, `roundtrip-E128` (new RC4 handler, read back by MuPDF) | 99.9% (5,035 / 5,039) each | 4 files whose streams have a crypt filter of their own, encrypted twice (D22), where the read-back depends on the random /ID. One more times out on both sides |
| `roundtrip-Eaes128`, `roundtrip-Eaes256` | 94.8% (4,775 / 5,039) each | The same 4, and 260 files with an empty stream, which AES output loses, so what is read back depends on the bytes after it |
| `clean-pages-reverse`, `clean-pages-first`, `clean-pages-rest` (`--pages N-1`, `1`, `2-N`) | 99.6% (4,958 / 4,978, 4,963 / 4,983, 4,958 / 4,978) | The 20 AES files. Both tools fail on 58 to 63 files more than `clean`, whose page trees cannot be walked |
| `meta-set`, `meta-strip` (`meta --set info:Title=nupdf --set Author=Été`, `--strip`) | 99.6% (5,021 / 5,041) each | The 20 AES files |
| `pages-rotate`, `pages-select-rotate` (`--rotate 90`; `--select N-1 --rotate -90 --rotate-pages 1`) | 99.6% (5,011 / 5,031, 4,957 / 4,977) | The 20 AES files. Both tools fail on 10 and 64 files more than `clean`, whose page trees cannot be walked |
| `meta-set-incremental`, `meta-strip-incremental`, `pages-rotate-incremental` | 99.6% (4,847 / 4,867), 99.9% (4,861 / 4,867), 100% (4,859 / 4,859) | AES files only: a rewritten object's strings get MuPDF's random IVs. Both tools refuse the 211 repaired files and the 45 locked ones, and fail on the 19 that do not open |
| `merge`, `merge-twice`, `merge-reverse` (pages, links, outlines) | 100% (5,123 / 5,123) each | The output is never encrypted, so no random IVs. A locked input, or one whose page tree fails, merges nothing or only the pages before the failure, in both tools; only the 19 files neither opens fail |
| `qpdf-clean`, `qpdf-clean-Z`, `qpdf-merge`, `qpdf-meta-incremental` (`qpdf --check` on each output: its verdict and messages) | 100% (5,040 / 5,040), 99.98% (5,017 / 5,018), 100% (5,123 / 5,123), 100% (4,867 / 4,867) | One AES file under `-Z`, where qpdf names an offset in an object stream whose bytes follow MuPDF's random IVs. MuPDF fails to save `issue20453.pdf`, as in `clean` |
| `arlington-clean`, `arlington-merge`, `arlington-meta-incremental` (selected Arlington rules on each output: the violations) | 100% (5,040 / 5,040, 5,123 / 5,123, 4,867 / 4,867) | |

`clean` reads its output back by default and refuses 7 standard files, all cases where MuPDF's output loses something (D18). With `-g` it refuses the same 7, and with `-gggg` also the 33 empty outputs (D19). With `-Z` it refuses one more file, an AES-256 document whose crypt filter dictionary ends up in an object stream it is needed to decrypt (D21); with `-Zgggg` the empty outputs open, as an xref stream with no objects, and the same 8 files are refused. With `-D` it refuses 2, which it refuses without `-D` too; with `-E rc4-128`, 6 of the 7, and with `-E aes-256`, 266, the files where MuPDF's encrypted output loses a stream (D22). The 45 files the empty password does not open fail before any of this, with `input.needs_password` (D24). On a 300-file sample, `nupdf clean` takes 614 ms in total against `mutool clean`'s 900 ms (geometric-mean speedup 1.50x), with that check included, `clean -gggg` 724 ms against 1,035 ms (1.48x), and `clean -z` 916 ms against 1,050 ms (1.36x), where the check decodes every stream. It is slower on files of a megabyte or more, where hashing the input and the output for their `doc_id` costs more than MuPDF's whole save. On the same sample, `nupdf merge` of each file with itself takes 677 ms against `mutool merge`'s 924 ms (1.44x), links and outline included, and `nupdf split` 935 ms against 1,518 ms for a script that does the same with MuPDF (1.94x, the script's overhead included); each is slower on 5 to 11 files, the larger ones, for the same reason.

On their own terms, qpdf finds nothing wrong with 4,779 of nupdf's 5,040 `clean` outputs, warns about 196 and reports errors in 65. In all but 8 the input gets the same verdict or a worse one. Of those 8, 2 hold an empty key name, which MuPDF writes lossily, so that `/` and the reference after it read back as a name. The other 6 are inputs qpdf repairs, where the output has no /Root or page tree, or a /Contents array item that is not a stream. The Arlington rules find violations in 201 outputs, all of them in the input too: pages without /Resources (117) or /MediaBox, trailers without /Root, page tree nodes missing required keys, and a string /Trapped.

Exit: ≥99.5% round-trip parity on the standard tier: MuPDF reads our output with the same object graph (modulo renumbering) and the same text. `merge`, `split` and `clean` are at least as fast as mutool.

Exit status: met for the object graph (`roundtrip` 99.96%) and for speed. The same text is checked once nupdf extracts text: M5's `roundtrip-text` check reads it from `clean`'s output.

## M4: Content streams, resources, fonts

MuPDF reference: `pdf-interpret.c`, `pdf-op-run.c`, `pdf-op-buffer.c`, `pdf-run.c`, `pdf-resources.c`, `pdf-xobject.c`, `pdf-pattern.c`, `pdf-shade.c`, `pdf-function.c`, `pdf-colorspace.c`, `pdf-image.c`, `pdf-layer.c`, `pdf-font.c`, `pdf-type3.c`, `pdf-cmap*.c`, `pdf-unicode.c`, `pdf-metrics.c`; `fitz/font.c`, `fitz/device.c`.

- [x] Content lexer and parser, including inline images with MuPDF's `EI` detection (DECISIONS D29): `pdf_process_stream` and `pdf_process_keyword` with MuPDF's operand stack and recovery, ExtGState entries, contents arrays joined as `fz_open_concat` joins them, and inline images decoded through a leech to find their end (`content`, with colorspace definitions in `colorspace`); the `operators` command
- [x] Interpreter: graphics and text state, CTM, `q`/`Q`, `BT`/`ET`, marked content, Form XObjects with recursion limits, shadings, patterns, inline images, optional-content visibility (DECISIONS D31): `pdf-op-run.c` ported whole, with soft masks, transparency groups, tiling patterns drawn as tiles or cell by cell, images as `pdf_load_image` loads them, layers, structure (with MuPDF's tree repair) and metatext, and a page run as `pdf_run_page_contents` makes it (`run`, `layer`, `path`, `image`). Text is shown with the font item
- [x] `Device` trait mirroring `fz_device`, plus a trace device whose output lines up with `mutool trace`: `device`, and the trace device of the `trace` command, checked against MuPDF's script device
- [x] Functions (types 0, 2, 3 and 4) and color-space definitions (conversion arrives in M7) (DECISIONS D30): `pdf-function.c` loaded and evaluated as MuPDF does, calculator functions included (`function`); colorspaces with their tint transforms, ICC profiles judged by their header (`colorspace`); shadings loaded, functions sampled, as `pdf_load_shading` does (`shade`). The interpreter loads them as `pdf_process_keyword` does, and the `operators-resources` check compares the result
- [x] Fonts (with them, text in the trace, and Type 3 glyph procedures) (DECISIONS D32): simple, composite and Type 3 fonts as `pdf_load_font` loads them (`fontdesc`), on a port of the parts of FreeType MuPDF uses to open TrueType, OpenType, CFF and Type 1 programs, map characters and measure glyphs (`font`); the standard 14 fonts and MuPDF's substitutes from the URW fonts it bundles; encodings, `/Differences`, the Adobe Glyph List, ToUnicode, predefined CMaps generated from MuPDF's tables (`cmap`, `glyphs`, `tables`, `cargo xtask tables`), `/W` and `/W2` widths, vertical writing; glyphs shown in spans as `fz_text` holds them (`text`), Type 3 glyphs prepared when their font loads and drawn through the device when they take the graphics state. Glyph outlines (and so exact glyph boxes) wait for M7, and CJK substitutes for a decision on carrying MuPDF's 25 MB CJK font
- [x] Commands: `trace`, `fonts`, `images` (list and extract) (DECISIONS D33): the fonts and images each page's resources reach, as `mutool info` walks them (`inventory`); embedded font programs and images written as `mutool extract` writes them; images decoded to pixmaps as `fz_get_pixmap_from_image` decodes them (`pixmap`), with MuPDF's PAM and PNG writers. JPEG 2000 images are listed but not decoded yet
- [x] Fuzz targets: content parser, font parsers. `content` processes any input as a page's contents, with resources of each kind and without: 15 minutes (839K runs), nothing found; `function` loads any input as a calculator or sampled function, evaluates it and samples it as an axial shading's: 15 minutes (8.7M runs), nothing found; `run` runs each page of any document into a device: 2 minutes (60K runs), nothing found; `font` opens any input as a font program, on its own or embedded in a simple or composite font of each kind, and asks for each glyph's name, width and mapping: 5 minutes (497K runs), nothing found; `cmap` loads any input as an embedded CMap and looks up the codes it splits itself into: 5 minutes (3.6M runs), nothing found; `pixmap` decodes any input as the samples of an image of each depth, colorspace, decode array, color key and /Matte mask, and writes it as PAM and PNG: 5 minutes (1.4M runs), nothing found

**Status** (standard tier, 5,142 files, of which 19 neither tool opens):

| Check | Parity | Remaining mismatches |
|---|---|---|
| `operators` (each page's operators and operands, its resources and annotations taken away) | 100% (5,123 / 5,123) | |
| `operators-resources` (the same with each page's resources kept, so the colorspaces, functions, shadings and patterns they name are loaded) | 100% (5,123 / 5,123) | |
| `operators-warnings` (distinct warnings and reported errors, resources taken away) | 99.5% (5,096 / 5,123) | 24 files that differ when they are opened, as in `warnings`. 2 where the oracle's own edits, made on a repaired document, make MuPDF's page tree map fail to load, and 1 where nupdf's inflate names a different zlib error |
| `trace` (each page's device calls, text included: spans, glyph ids, Unicode and positions) | 99.5% (5,098 / 5,123) | All 25 are CJK fonts that are not embedded: MuPDF's build carries a CJK font to substitute and nupdf does not (DECISIONS D32) |
| `fonts` (the fonts each page's resources reach: page, object, subtype, name, encoding) | 100% (5,123 / 5,123) | |
| `font-files` (each embedded font program as `mutool extract` writes it) | 100% (5,123 / 5,123) | |
| `image-list` (the images each page's resources reach: page, object, size, depth, filters, colorspace) | 100% (5,123 / 5,123) | |
| `image-pixmaps` (each image XObject decoded to a pixmap: size, components, alpha, colorspace, MD5 of the samples) | 99.2% (5,081 / 5,123) | 27 files with JPEG 2000 images, which nupdf does not decode yet; 14 whose JBIG2 streams already decode differently (as in `images`); 1 with a `/Length 0` stream behind two filters, where MuPDF's image read sees the Flate data cut short and nupdf's does not |

Exit: ≥99% page-trace parity on the standard tier: same operations, matrices within 1e-3, same glyph ids and Unicode.

## M5: Text extraction and search

MuPDF reference: `fitz/stext-device.c`, `stext-output.c`, `stext-search.c`, `stext-iterator.c`, `stext-para.c`, `stext-classify.c`, `stext-table.c`, `stext-boxer.c`, `stext-raft.c`.

- [x] Structured text model (pages, blocks, lines, chars) with MuPDF's options: preserve ligatures, preserve whitespace, preserve spans, dehyphenate, clip to mediabox
- [x] Output: JSON with stable ids, plain text, Markdown, JSONL per page, with output budgets
- [x] Search: literal and regex, case- and diacritic-insensitive, across hyphenation and line breaks; hits carry quads and span ids
- [x] Paragraph, column and table segmentation (`stext-para`, `stext-boxer`, `stext-table`), at parity first
- [x] Page-parallel extraction with output identical at every thread count
- [x] Opt-in improved reading order, scored against hand-labeled ground truth rather than MuPDF
- [x] Bench pairs: against `mutool draw -F stext.json`, plus, for context, PyMuPDF, pypdf, pdfplumber, pdfminer.six, pdftotext and pdf_oxide
- [x] `roundtrip-text` check: MuPDF's text of each side's `clean` output, the same on both (moved from M3's exit)

Exit: ≥99% of pages have the same character sequence as MuPDF's stext, and ≥98% of characters have bbox IoU ≥0.98. Single-threaded time ≤1.1× MuPDF's. ≥6× speedup on 8 cores, in total time, for documents of 50+ pages that take 100 ms or more on one thread (`stext-scaling`, `extract-scaling`; a document that is mostly starting the process shows nothing of what threads do).

Exit status: met for parity and single-threaded time. Scaling, 2026-11-12, on 6 performance and 12 efficiency cores with Finder, fseventsd and Spotlight each holding a core: on the standard tier `stext` 3.4× (one document qualifies), `extract` 2.8× (two); on the `long` tier (30 documents of 100 pages or more merged from the standard tier's files, `cargo xtask corpus fetch long`; 9 qualify) `stext` 2.36× in geometric mean and 1.29× in total time, `extract` 2.33× and 1.25×. The total is one document's: a single page of `long-006.pdf` takes 2.3 s (MuPDF 2.0 s), which threads over pages cannot split; the others run 2.1× to 3.4×.

## M6: Redaction and verification

MuPDF reference: `pdf-clean.c` (`pdf_redact_page` and helpers), `pdf-op-filter.c`, `pdf-image-rewriter.c`, `pdf-annot.c` (Redact annotations), `pdf-form.c`, `pdf-outline.c`, `pdf-nametree.c`.

- [x] Targets: text, regex, presets (email, phone, card-like, SSN-like, IBAN), bboxes, span ids, search-hit files; resolved to quads per page
- [x] Content filtering with MuPDF's policies: text removed; images left, removed or pixel-blanked; line art left, removed when covered, or removed when touched. Split `TJ` arrays so surrounding text keeps its position; give pages their own copies of shared Form XObjects; handle Type3 glyphs and clipping paths
- [x] Image pixel redaction: decode, blank, re-encode with the original filter where possible
- [x] Annotations, widgets and links inside redacted regions; optional overlay boxes
- [x] Document scrub: Info, XMP, outline titles, named destinations, embedded files, form values, JavaScript, ActualText, Alt and structure tree, hidden optional content, thumbnails, unused objects, incremental history (always a full rewrite)
- [x] `verify`: an independent pass over the output that checks every leak class in [AGENT-INTERFACE](AGENT-INTERFACE.md#redaction-contract)
- [x] Adversarial fixture generator, one fixture per leak class; the `redaction` corpus tier (pdf-redaction-benchmarks). `cargo xtask redaction`: 23 fixtures in `corpus/redaction`, each seen leaking in its class before and judged clean after by `nupdf verify`, MuPDF and pdftotext; `--benchmarks`: the generator's 20 cases for seeds 1 to 10 (about 1,400 probes, text and text drawn as images), zero leaks and every distractor kept
- [x] `--dry-run`: the redaction plan as quads now, with rendered previews after M7

Exit: zero leaks on the adversarial suite and the redaction tier, as judged by `nupdf verify`, MuPDF extraction and `pdftotext`. Remaining-text parity with MuPDF's redaction ≥99%. Tests seeded with deliberately leaky outputs prove `verify` fails them. Measured: zero leaks on both; `redact-text` 99.7% (5,052 / 5,066).

## M7: Rendering

MuPDF reference: `fitz/draw-*.c`, `colorspace.c`, `color-lcms.c`, `separation.c`, `pdf-shade.c`, glyph caching in `fitz/font.c`.

- [x] Rasterizer decision: vello_cpu, tiny-skia, or a port of `draw-edge`/`draw-paint`, chosen by measured pixel parity and speed. A port (DECISIONS D41): MuPDF's anti-aliasing, FreeType's glyph rasterizer and MuPDF's 8-bit painters put any other rasterizer outside the threshold on most text
- [x] Paths and strokes (joins, caps, dashes), clipping, glyphs (outlines and Type3), images (every color space, decode arrays, stencil and soft masks, interpolation), shadings 1–7, patterns, transparency groups, soft masks, blend modes, knockout, overprint simulation, ICC through moxcms. Overprint: MuPDF simulates it only for subtractive destinations, and `render` draws RGB, so there is nothing to simulate; ICC: moxcms with lcms2's black point compensation, within 1 of MuPDF on SWOP CMYK. Known differences in D41
- [x] Band-parallel rendering within a page, and page-parallel across pages: `--threads` runs pages in parallel and plays a large page's display list into bands, with identical pixels
- [x] `render`: PNG output for a page or region, set DPI, and `--max-pixels` to fit vision-model budgets (also PPM, and `--no-icc` for `mutool draw -N`)
- [x] Redaction previews for `--dry-run`: `--preview DIR` draws each page with targets, the areas shaded red
- [x] hayro as a second reference renderer, for triaging MuPDF disagreements: `cargo xtask triage FILE --page N` renders with MuPDF, nupdf and hayro and gives each pair's distance

Exit: ≥98% of standard-tier pages within the pixel threshold of `mutool draw` at 72 and 150 dpi. Single-threaded time ≤1.2× MuPDF's, and faster than MuPDF on multiple cores. Measured (a pixel off by more than 8 of 255 in a channel, at most 0.5% of a page's pixels off; page contents without annotations): with color management on, as `mutool draw` has it, 98.98% of 19,790 pages at 72 dpi and 98.40% of 9,790 at 150 dpi (the pages MuPDF renders within the harness's time limit); with it off on both sides, 99.30% and 99.03%. Speed against `mutool draw -N` at 72 dpi on 200 standard-tier files: one thread, geometric mean 1.25× MuPDF's speed (total 1,293 ms against 1,357 ms); eight threads, 1.28× (the oracle is built without threads, so MuPDF runs on one).

## M8: Agent surface

- [x] MCP server over stdio: tools mirror the commands, a document-handle cache avoids reparsing, limits and cancellation apply per call. `nupdf mcp` (D42): every command a tool, its schema read from the CLI's arguments; reports byte for byte the CLI's; files' bytes and ids kept between calls, documents parsed per call so warnings match the CLI's; `timeout`, cancellation and an output ceiling per call, and `--timeout` for the CLI
- [x] `batch`: globs, parallel execution, JSONL output, resumable runs. `nupdf batch` (D43): files, directories and globs; `--jobs` at once with lines in input order, the same at every count; `-o` and `--resume` take a run up again, rerunning files that changed; `{stem}`/`{name}`/`{index}` name outputs; `--item-timeout`
- [x] `diff`: compare two PDFs by text, structure and pixels. `nupdf diff` (D44): pages lined up by their text, then lines with content ids on each side; structure as named parts; pixels with counts and boxes in points
- [x] `guide` (compact usage text for models), `schema <command>` (JSON Schema), and a `SKILL.md` for Claude Code. D45: the guide and `skills/nupdf/SKILL.md` made from the CLI's definition; 27 schemas read off the reports on the smoke tier and 400 standard-tier files by `cargo xtask schemas`, checked against 150 others
- [x] Freeze schemas at v1, with a changelog. Every report is `nupdf.<command>/v1`; [SCHEMA-CHANGELOG](SCHEMA-CHANGELOG.md) says what a version promises and what changes it
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
